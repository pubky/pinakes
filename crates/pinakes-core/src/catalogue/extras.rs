//! The extras of a card built from a catalogue card that are read from other services, in the
//! background, and kept in the stored card: its Wikipedia article's summary as its description,
//! credited to Wikipedia. Also on needs nothing here: the card's save queues its also-on sources
//! as any saved card's.
//!
//! A read is due when the card is built from a card or an article it has no summary of yet
//! (`join`), and when someone opens it after [`KEEP`] (`opened`). Only a stored card has extras: a
//! read of a card nobody shared asks no other service.
use super::join::CardRow;
use crate::{
    model::{CreditField, CreditProvider, ResourceMeta},
    resolver::egress::Egress,
    wikimedia::Found,
};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

/// How long a summary read is kept before opening the card reads it again.
pub const KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 86400);
/// The longest wait after a read that failed for a while.
const MAX_RETRY_SECS: u64 = 86400;

/// The Wikipedia article whose summary a card takes: its own link when that is an article (a
/// Wikidata card's), else the article its details name (`details.wikipedia`).
pub fn article(card: &CardRow) -> Option<String> {
    [Some(card.uri.as_str()), card.article.as_deref()]
        .into_iter()
        .flatten()
        .find_map(|uri| crate::wikipedia::parse(uri).map(|a| a.canonical))
}
/// Mark `id`'s extras due now: its card was built from a card or an article it has no summary
/// of. A read of the same article within [`KEEP`] (it found no summary) is not asked again.
pub(crate) async fn mark(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    article: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO catalogue_extras(resource_id,due_at,article) VALUES($1,now(),$2) ON CONFLICT (resource_id) DO UPDATE SET due_at=now(),attempts=0,article=excluded.article
        WHERE catalogue_extras.article IS DISTINCT FROM excluded.article OR catalogue_extras.read_at IS NULL OR catalogue_extras.read_at<now()-$3*interval '1 second'")
        .bind(id)
        .bind(article)
        .bind(KEEP.as_secs() as f64)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Someone opened `id`, a card built from a catalogue card: its extras are read again when the
/// last read is older than [`KEEP`] and none is due. Writes nothing (no row lock) when none is
/// due. Whether one was marked.
pub async fn opened(pool: &PgPool, id: &str) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO catalogue_extras(resource_id,due_at) SELECT id,now() FROM resource WHERE id=$1 AND catalogue_card IS NOT NULL AND NOT EXISTS(SELECT 1 FROM catalogue_extras WHERE resource_id=$1) ON CONFLICT (resource_id) DO NOTHING",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if inserted > 0 {
        return Ok(true);
    }
    Ok(sqlx::query(
        "UPDATE catalogue_extras SET due_at=now(),attempts=0 WHERE resource_id=$1 AND due_at IS NULL AND (read_at IS NULL OR read_at<now()-$2*interval '1 second')",
    )
    .bind(id)
    .bind(KEEP.as_secs() as f64)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}
/// Keep in `doc`, built again from its card, the summary `stored` (the card before) took.
pub fn keep_summary(doc: &mut ResourceMeta, stored: &ResourceMeta) {
    if stored.description.is_none() {
        return;
    }
    doc.description = stored.description.clone();
    doc.credits.retain(|c| c.field != CreditField::Description);
    doc.credits.extend(
        stored
            .credits
            .iter()
            .filter(|c| c.field == CreditField::Description)
            .cloned(),
    );
    if let (Some(c), Some(s)) = (doc.catalogue.as_mut(), stored.catalogue.as_ref()) {
        c.summary = s.summary.clone();
    }
}
/// `doc` with its description the summary `text` of `article`, credited by `credit` unless the
/// card's own link is the article (then the text is the link's own); `None` when it is no card
/// built from a catalogue card, the text carries a credential, or the card would not fit.
fn with_summary(
    doc: &ResourceMeta,
    text: String,
    credit: crate::model::Credit,
    article: &str,
) -> Option<ResourceMeta> {
    doc.catalogue.as_ref()?;
    if crate::capability::contains_credential(&text) {
        return None;
    }
    let mut doc = doc.clone();
    let own = crate::wikipedia::parse(&doc.url).map(|a| a.canonical) == Some(article.to_owned());
    doc.description = Some(text);
    doc.credits.retain(|c| c.field != CreditField::Description);
    if !own && credit.provider == CreditProvider::Wikipedia {
        doc.credits.push(credit);
    }
    if let Some(c) = doc.catalogue.as_mut() {
        c.summary = Some(article.to_owned());
    }
    let own_uri = doc.url.clone();
    crate::capability::strip_document(&mut doc, &own_uri).ok()?;
    doc.validate(&[]).ok()?;
    Some(doc)
}

/// Read the extras of up to `limit` cards that are due, through `egress` (paced and cached as
/// every request). A card no longer built from a catalogue card loses its row. A read that
/// failed, or whose write failed, is tried again later, backing off to a day; the others go on.
/// The rows read.
pub async fn run_due(pool: &PgPool, egress: &Egress, limit: i64) -> Result<usize> {
    let due: Vec<(String, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT e.resource_id,e.attempts,c.uri,c.details->>'wikipedia' FROM catalogue_extras e JOIN resource r ON r.id=e.resource_id LEFT JOIN catalogue_card c ON c.id=r.catalogue_card WHERE e.due_at<=now() ORDER BY e.due_at,e.resource_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for (id, attempts, uri, named) in &due {
        if uri.is_none() {
            // No longer built from a card.
            sqlx::query("DELETE FROM catalogue_extras WHERE resource_id=$1")
                .bind(id)
                .execute(pool)
                .await?;
            continue;
        }
        let page = [uri.as_deref(), named.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|u| crate::wikipedia::parse(u).map(|a| a.canonical));
        let found = match &page {
            Some(page) => crate::music_enrichment::article_summary(egress, page).await,
            None => Found::Missing,
        };
        let written = match (found, page.clone()) {
            (Found::Value(text, credit), Some(page)) => crate::changes::rewrite(
                pool,
                id,
                || async { anyhow::Ok(()) },
                |doc, ()| match with_summary(doc, text, credit, &page) {
                    Some(with) => {
                        let changed = with.description != doc.description
                            || with.credits != doc.credits
                            || with.catalogue != doc.catalogue;
                        *doc = with;
                        changed
                    }
                    None => false,
                },
            )
            .await
            .map(|_| true),
            (Found::Unread, _) => Ok(false),
            _ => Ok(true),
        };
        match written {
            Ok(true) => {
                sqlx::query("UPDATE catalogue_extras SET read_at=now(),due_at=NULL,attempts=0,article=$2 WHERE resource_id=$1")
                    .bind(id)
                    .bind(&page)
                    .execute(pool)
                    .await?;
            }
            failed => {
                // Unread (Wikipedia did not answer) or not written: counted, read again later.
                crate::telemetry::catalogue_join_error("extras");
                if let Err(error) = failed {
                    tracing::warn!(resource_id = %id, kind = crate::resolver::error_class(&error), "a catalogue card's summary was not written; it is read again later");
                }
                let wait = 60u64
                    .saturating_mul(1 << (*attempts).clamp(0, 12))
                    .min(MAX_RETRY_SECS);
                sqlx::query("UPDATE catalogue_extras SET attempts=attempts+1,due_at=now()+$2*interval '1 second' WHERE resource_id=$1")
                    .bind(id)
                    .bind(wait as f64)
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(due.len())
}
