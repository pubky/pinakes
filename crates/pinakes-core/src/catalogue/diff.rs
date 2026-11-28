//! Writing a catalogue that is read again in full on each new version of its file (Podcast
//! Index's weekly database, the daily package catalogues, an OpenAlex snapshot's new parts):
//! [`Sink`].
//!
//! In a full import every card is written through [`Job::write`], and the cards the import did
//! not read are removed when it finishes (`store::finish_import`). In an update the file is read
//! again, but only the cards that are new or whose content changed are written: each part's
//! stored content hashes are read first, so an unchanged row is never touched. The update then
//! removes the stored cards of its scope it did not read, unless they are more than a tenth of
//! the scope's cards ([`too_many`]): such an update is refused ([`Finished::Refused`]), since a
//! broken or cut file must never empty a catalogue. The importer then refuses the file
//! ([`Job::refuse`]: it is named in the position, the run abandoned, `refused` recorded once);
//! its next runs find the same file refused and read nothing ([`Job::refused_before`]) until a
//! newer one appears, a full import is asked for (`pinakesd catalogue import <name> --request`,
//! which the update hands over to) or the refusal is cleared (`pinakesd catalogue
//! clear-refusal <name>`).
use super::{run::Job, store::WriteReport, Card, Catalogue};
use anyhow::Result;
use sqlx::PgPool;

/// The share of a scope's cards an update may remove: more than one in [`REMOVAL_SHARE`] (and
/// more than [`REMOVAL_FLOOR`] cards) refuses the update.
pub const REMOVAL_SHARE: u64 = 10;
/// The removals an update may always make, whatever the share.
pub const REMOVAL_FLOOR: u64 = 10;
/// Stored source IDs read per query while removals are looked for.
const PAGE: i64 = 10_000;

/// Where a run's cards go: every card in a full import; only new and changed cards in an
/// update, which also removes the scope's cards it did not read ([`Sink::finish`]).
pub struct Sink {
    update: bool,
    /// The cards of one sub-source (`catalogue_card.source`), or of the whole catalogue.
    source: Option<String>,
    /// A 64-bit hash of each source ID read (an update's removals are the stored IDs not here).
    seen: Vec<u64>,
    /// Cards read and found unchanged (an update writes nothing for them).
    pub unchanged: u64,
    /// Stored source IDs read per query while removals are looked for ([`PAGE`]).
    page: i64,
}
/// What an update's last part decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finished {
    /// The scope's cards not read are removed (none in a full import).
    Removed(u64),
    /// They are more than a tenth of the scope: nothing removed, the update is refused.
    Refused,
}
impl Sink {
    /// The sink of `job`'s run, for the cards of `source` (`None`: the whole catalogue).
    pub fn new(job: &Job<'_>, source: Option<&str>) -> Self {
        Self {
            update: job.run() == super::run::Run::Update,
            source: source.map(str::to_owned),
            seen: Vec::new(),
            unchanged: 0,
            page: PAGE,
        }
    }
    /// Write `cards` (one part, at most a few thousand) and save `cursor` with them. In an
    /// update, only the cards whose stored content hash differs (or that are not stored) are
    /// written; the unchanged and the refused ([`Card::checked`]) are counted as a full import
    /// counts them (`pinakes_catalogue_cards_total`), each refusal logged. A refused card's stored
    /// copy stays (it counts as read): a record that no longer makes a card removes nothing.
    pub async fn write(
        &mut self,
        job: &mut Job<'_>,
        cards: Vec<Card>,
        cursor: serde_json::Value,
    ) -> Result<WriteReport> {
        if !self.update {
            return job.write(&cards, cursor).await;
        }
        let catalogue = job.catalogue();
        let mut checked = Vec::with_capacity(cards.len());
        let mut rejected = 0;
        for card in cards {
            match card.checked(catalogue) {
                Ok(row) => {
                    self.seen.push(id_hash(&row.source_id));
                    checked.push((card, row.content_hash));
                }
                Err(reject) => {
                    rejected += 1;
                    self.seen.push(id_hash(&card.source_id));
                    // Its ID as far as a valid one goes, as `store::upsert` logs it.
                    let mut shown: String = card.source_id.chars().take(128).collect();
                    shown.retain(|c| !c.is_control());
                    tracing::debug!(catalogue = catalogue.name(), source_id = %shown, ?reject, "catalogue card rejected");
                }
            }
        }
        job.count_rejected(rejected);
        let ids: Vec<&str> = checked
            .iter()
            .map(|(card, _)| card.source_id.as_str())
            .collect();
        let stored: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT source_id,content_hash FROM catalogue_card WHERE catalogue=$1 AND source_id=ANY($2)",
        )
        .bind(catalogue.name())
        .bind(&ids)
        .fetch_all(job.pool())
        .await?;
        let stored: std::collections::HashMap<String, Vec<u8>> = stored.into_iter().collect();
        let mut changed = Vec::new();
        let mut unchanged = 0;
        for (card, hash) in checked {
            if stored.get(&card.source_id).map(Vec::as_slice) == Some(&hash[..]) {
                unchanged += 1;
            } else {
                changed.push(card);
            }
        }
        self.unchanged += unchanged;
        let mut written = job.write(&changed, cursor).await?;
        written.unchanged += unchanged;
        written.rejected += rejected;
        job.count_unchanged(unchanged);
        crate::telemetry::catalogue_cards(
            catalogue.name(),
            WriteReport {
                unchanged,
                rejected,
                ..WriteReport::default()
            },
        );
        Ok(written)
    }
    /// The end of the run's reading. In an update: remove the scope's stored cards it did not
    /// read, or refuse ([`Finished::Refused`], nothing removed) when they are more than a tenth
    /// of the scope; `removing` is the cursor saved with the removals. Nothing to do in a full
    /// import.
    pub async fn finish(
        mut self,
        job: &mut Job<'_>,
        removing: serde_json::Value,
    ) -> Result<Finished> {
        if !self.update {
            return Ok(Finished::Removed(0));
        }
        self.seen.sort_unstable();
        self.seen.dedup();
        let pool = job.pool().clone();
        let catalogue = job.catalogue();
        // First count, then remove: nothing is removed by an update that is refused.
        let (stored, gone) = self.unseen(&pool, catalogue).await?;
        if too_many(gone, stored) {
            tracing::warn!(
                catalogue = catalogue.name(),
                source = self.source.as_deref().unwrap_or(""),
                stored,
                gone,
                "catalogue update refused: it would remove more than a tenth of the cards"
            );
            return Ok(Finished::Refused);
        }
        let mut removed = 0;
        let mut after: Option<String> = None;
        loop {
            let ids = self.unseen_page(&pool, catalogue, after.as_deref()).await?;
            let Some((last, ids)) = ids else { break };
            if !ids.is_empty() {
                removed += job.remove(&ids, removing.clone()).await?;
            }
            after = Some(last);
        }
        Ok(Finished::Removed(removed))
    }
    /// The scope's stored cards and how many of them were not read.
    async fn unseen(&self, pool: &PgPool, catalogue: Catalogue) -> Result<(u64, u64)> {
        let mut stored = 0;
        let mut gone = 0;
        let mut after: Option<String> = None;
        loop {
            let page = self.page(pool, catalogue, after.as_deref()).await?;
            let Some(last) = page.last().cloned() else {
                break;
            };
            stored += page.len() as u64;
            gone += page.iter().filter(|id| !self.was_seen(id)).count() as u64;
            after = Some(last);
        }
        Ok((stored, gone))
    }
    /// One page of the scope's source IDs after `after`, and those of them not read; `None`
    /// past the last.
    async fn unseen_page(
        &self,
        pool: &PgPool,
        catalogue: Catalogue,
        after: Option<&str>,
    ) -> Result<Option<(String, Vec<String>)>> {
        let page = self.page(pool, catalogue, after).await?;
        let Some(last) = page.last().cloned() else {
            return Ok(None);
        };
        Ok(Some((
            last,
            page.into_iter().filter(|id| !self.was_seen(id)).collect(),
        )))
    }
    async fn page(
        &self,
        pool: &PgPool,
        catalogue: Catalogue,
        after: Option<&str>,
    ) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT source_id FROM catalogue_card WHERE catalogue=$1 AND ($2::text IS NULL OR source=$2) AND ($3::text IS NULL OR source_id>$3) ORDER BY source_id LIMIT $4",
        )
        .bind(catalogue.name())
        .bind(self.source.as_deref())
        .bind(after)
        .bind(self.page)
        .fetch_all(pool)
        .await?)
    }
    fn was_seen(&self, id: &str) -> bool {
        self.seen.binary_search(&id_hash(id)).is_ok()
    }
}
fn id_hash(id: &str) -> u64 {
    let hash = blake3::hash(id.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes"))
}
/// Where `input` reads instead of the catalogue's own file at `official` (`Importer::source`):
/// none when it is that file.
pub fn source_of(input: &super::fetch::Input, official: &str) -> Option<String> {
    match input {
        super::fetch::Input::Url(url) if url.as_str() == official => None,
        super::fetch::Input::Url(url) => Some(url.to_string()),
        super::fetch::Input::File(file) => Some(file.display().to_string()),
    }
}
/// Whether removing `gone` of `stored` cards is more than an update may: more than
/// [`REMOVAL_FLOOR`] cards and more than one in [`REMOVAL_SHARE`]. Such an update is refused.
pub fn too_many(gone: u64, stored: u64) -> bool {
    gone > REMOVAL_FLOOR && gone.saturating_mul(REMOVAL_SHARE) > stored
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The removals are found page by page: a scope of 25 stored cards of which 20 were read,
    /// in pages of 10, names exactly the 5 not read, and counts the scope whole.
    #[sqlx::test(migrations = "../../migrations")]
    async fn removals_are_found_past_one_page(pool: PgPool) {
        let cards: Vec<Card> = (0..25)
            .map(|n| Card {
                source: "flathub".into(),
                source_id: format!("flathub/org.example.App{n:02}"),
                uri: format!("https://flathub.org/apps/org.example.App{n:02}"),
                r#type: "SoftwareApplication".into(),
                name: format!("App {n}"),
                ..Card::default()
            })
            .collect();
        let written = super::super::store::upsert(&pool, Catalogue::Software, 1, &cards, None)
            .await
            .unwrap();
        assert_eq!(written.inserted, 25);
        let mut sink = Sink {
            update: true,
            source: Some("flathub".into()),
            seen: cards[..20].iter().map(|c| id_hash(&c.source_id)).collect(),
            unchanged: 0,
            page: 10,
        };
        sink.seen.sort_unstable();
        assert_eq!(
            sink.unseen(&pool, Catalogue::Software).await.unwrap(),
            (25, 5)
        );
        let mut gone = Vec::new();
        let mut after: Option<String> = None;
        let mut pages = 0;
        while let Some((last, ids)) = sink
            .unseen_page(&pool, Catalogue::Software, after.as_deref())
            .await
            .unwrap()
        {
            pages += 1;
            gone.extend(ids);
            after = Some(last);
        }
        assert_eq!(pages, 3);
        let want: Vec<String> = cards[20..].iter().map(|c| c.source_id.clone()).collect();
        assert_eq!(gone, want);
    }
}
