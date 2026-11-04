//! A MusicBrainz card's image and description, from the entity's own Wikidata item: its English
//! Wikipedia article's summary and its image on Wikimedia Commons, each with its credit
//! (`model::Credit`).
//!
//! Only exact identifiers lead there, never a name: the entity's own `wikidata` URL relation
//! names the item, and the item names the article (its `enwiki` sitelink) and the image (`P18`).
//! A release and its release group each use their own relation. A recording without any may take
//! the description of the one work it is a performance of, when that work has its own relation;
//! the credit then names the work (`subject`), and the work's image is never the recording's.
//!
//! The article's summary is used as plain text, never its thumbnail (an album cover there is
//! usually a non-free file of Wikipedia's own). The image is shown only when Commons says the
//! file is free to show with its credit (`wikimedia::free_file`).
//!
//! Each field ends in one of three ways: a value with its credit; missing (no item, an article
//! that is not there (404, 410), a sitelink to a redirect, a disambiguation page, no image, a
//! refused file), which leaves the card as MusicBrainz alone makes it; or unread (a timeout, a
//! 429, a 5xx, any other status), which keeps what the stored card holds
//! (`ResourceMeta::keep_enriched`). Every request goes through the egress, whose shared flights
//! and cache make concurrent cards of one entity ask once.
use crate::{
    hubs::{musicbrainz, wikidata, MbKind},
    model::{Credit, CreditField, CreditProvider, ResourceMeta},
    resolver::egress::{Egress, FetchOptions},
};
use serde_json::Value;

/// The licence of Wikipedia's text.
const WIKIPEDIA_LICENSE: (&str, &str) = (
    "CC-BY-SA-4.0",
    "https://creativecommons.org/licenses/by-sa/4.0/",
);
/// How long an answer stays in the egress cache (seconds); a card is read again at its own
/// refresh.
const CACHE_SECS: u64 = 3600;

/// One field's outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    Value(String, Credit),
    /// Nothing to show: the card keeps MusicBrainz's own value.
    Missing,
    /// Not read for a transient failure: the card keeps its stored value.
    Unread,
}
/// The image and description found for one entity.
#[derive(Clone, Debug, PartialEq)]
pub struct Enrichment {
    pub image: Found,
    pub description: Found,
}
impl Enrichment {
    pub fn missing() -> Self {
        Self {
            image: Found::Missing,
            description: Found::Missing,
        }
    }
    pub fn unread() -> Self {
        Self {
            image: Found::Unread,
            description: Found::Unread,
        }
    }
    /// Writes the found values and their credits into `doc`. An unread field is listed in
    /// `unread_enriched`, and the card is read again early, `retry_secs` from now at the soonest
    /// (`ResourceMeta::keep_enriched` moves that later by how old its stored reading is).
    pub fn apply(self, doc: &mut ResourceMeta, retry_secs: u64) {
        for (field, found) in [
            (CreditField::Image, self.image),
            (CreditField::Description, self.description),
        ] {
            match found {
                Found::Value(value, credit) => {
                    match field {
                        CreditField::Image => doc.image = Some(value),
                        CreditField::Description => doc.description = Some(value),
                    }
                    doc.credits.retain(|c| c.field != field);
                    doc.credits.push(credit);
                }
                Found::Missing => {}
                Found::Unread => {
                    doc.unread_enriched.push(field);
                    let at = chrono::Utc::now()
                        + chrono::Duration::seconds(retry_secs.min(i64::MAX as u64) as i64);
                    doc.refresh_by = Some(doc.refresh_by.map_or(at, |by| by.min(at)));
                }
            }
        }
        doc.credits.sort_by_key(|c| c.field);
    }
}

/// The Wikidata item a list of URL relations names.
#[derive(Debug, PartialEq)]
enum Own {
    /// Exactly one item.
    Item(String),
    /// No `wikidata` relation.
    None,
    /// `wikidata` relations that name several items, or none that is an item.
    Unclear,
}
fn own_item(urls: &[musicbrainz::UrlRelation]) -> Own {
    let relations: Vec<&musicbrainz::UrlRelation> =
        urls.iter().filter(|r| r.kind == "wikidata").collect();
    let mut items: Vec<String> = relations
        .iter()
        .filter_map(|r| wikidata::item_link(&r.url).map(|(qid, _)| qid))
        .collect();
    items.sort();
    items.dedup();
    match items.as_slice() {
        [one] => Own::Item(one.clone()),
        _ if relations.is_empty() => Own::None,
        _ => Own::Unclear,
    }
}

/// The image and description of `entity` (`base`: the MusicBrainz web service), each
/// thumbnail `width` pixels wide.
pub async fn enrich(
    egress: &Egress,
    base: &str,
    entity: &musicbrainz::Entity,
    width: u32,
) -> Enrichment {
    match own_item(&entity.urls) {
        Own::Item(qid) => return item(egress, &entity.mbid, &qid, None, width).await,
        // A recording whose own relations are unclear is not its work.
        Own::Unclear => return Enrichment::missing(),
        Own::None if entity.kind != MbKind::Recording => return Enrichment::missing(),
        Own::None => {}
    }
    let works = match musicbrainz::works(egress, base, &entity.mbid).await {
        Ok(works) => works,
        Err(error) => {
            unread(&entity.mbid, "musicbrainz_works", &error);
            return Enrichment::unread();
        }
    };
    let [work] = works.as_slice() else {
        return Enrichment::missing();
    };
    let Own::Item(qid) = own_item(&work.urls) else {
        return Enrichment::missing();
    };
    let subject = format!("https://musicbrainz.org/work/{}", work.mbid);
    Enrichment {
        image: Found::Missing,
        description: description(egress, &entity.mbid, &qid, Some(subject)).await,
    }
}

async fn item(
    egress: &Egress,
    mbid: &str,
    qid: &str,
    subject: Option<String>,
    width: u32,
) -> Enrichment {
    let (description, image) = tokio::join!(
        description(egress, mbid, qid, subject),
        image(egress, mbid, qid, width)
    );
    Enrichment { image, description }
}

/// Logs a transient failure by its class, with the entity and the step; never a URL.
fn unread(mbid: &str, step: &'static str, error: &anyhow::Error) {
    tracing::info!(
        mbid,
        step,
        kind = crate::resolver::error_class(error),
        "music enrichment left a field unread"
    );
}

fn json_options() -> FetchOptions {
    FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    }
}

/// The item's English Wikipedia summary.
async fn description(egress: &Egress, mbid: &str, qid: &str, subject: Option<String>) -> Found {
    let page = match wikidata::sitelink(egress, qid, "enwiki").await {
        Ok(Some(page)) => page,
        Ok(None) => return Found::Missing,
        Err(error) => {
            unread(mbid, "wikidata_sitelink", &error);
            return Found::Unread;
        }
    };
    let Some(article) = crate::wikipedia::parse(&page).filter(|a| a.lang == "en") else {
        return Found::Missing;
    };
    let fetched = match egress
        .discover(
            &crate::wikipedia::summary_url(&article),
            &json_options(),
            CACHE_SECS,
        )
        .await
    {
        Ok(fetched) => fetched,
        Err(error) => {
            unread(mbid, "wikipedia_summary", &error);
            return Found::Unread;
        }
    };
    match fetched.status {
        200 => {}
        // No such article.
        404 | 410 => return Found::Missing,
        // A wait, or a refusal of this client (a 403 for its User-Agent): no answer about the
        // article.
        status => {
            tracing::info!(mbid, status, "music enrichment: Wikipedia did not answer");
            return Found::Unread;
        }
    }
    let Ok(v) = serde_json::from_slice::<Value>(&fetched.body) else {
        tracing::info!(mbid, "music enrichment: Wikipedia's summary is not JSON");
        return Found::Unread;
    };
    let Some(text) = summary_text(&v, &article.title) else {
        return Found::Missing;
    };
    let (text, _) = excerpt(&text, crate::model::DESCRIPTION_CAP);
    let credit = Credit {
        field: CreditField::Description,
        provider: CreditProvider::Wikipedia,
        source_url: article.canonical.clone(),
        author: Some("Wikipedia contributors".into()),
        license: WIKIPEDIA_LICENSE.0.into(),
        license_url: Some(WIKIPEDIA_LICENSE.1.into()),
        lang: Some(article.lang.clone()),
        // A summary is the article's first paragraphs, cut further when long.
        excerpted: true,
        modified: false,
        subject,
    };
    match credit.bounded() {
        Some(credit) => Found::Value(text, credit),
        None => Found::Missing,
    }
}

/// An article summary's plain text: a standard article's `extract`, when the summary is of
/// `title` (with `_` for spaces) itself; nothing for a disambiguation page, an article without
/// one, or the page a redirect leads to (the summary follows redirects: a song's title may lead
/// to its album's article).
pub fn summary_text(v: &Value, title: &str) -> Option<String> {
    if v["type"].as_str() != Some("standard") || v["titles"]["canonical"].as_str() != Some(title) {
        return None;
    }
    v["extract"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// `text` within `max` bytes, and whether it was cut: at the end of the last sentence that fits,
/// else after the last word that fits, followed by "…".
pub fn excerpt(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    // A sentence ends with `.`, `!` or `?` before a space, within the bytes allowed.
    let sentence = head
        .char_indices()
        .rev()
        .find(|(i, c)| {
            matches!(c, '.' | '!' | '?')
                && text[i + c.len_utf8()..].starts_with(char::is_whitespace)
        })
        .map(|(i, c)| i + c.len_utf8());
    if let Some(end) = sentence.filter(|end| *end >= max / 2) {
        return (text[..end].to_owned(), true);
    }
    let room = max - "…".len();
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // A word that ends where the room does is whole.
    let word = if text[end..].starts_with(char::is_whitespace) {
        end
    } else {
        text[..end]
            .rfind(char::is_whitespace)
            .filter(|i| *i > 0)
            .unwrap_or(end)
    };
    (format!("{}…", text[..word].trim_end()), true)
}

/// The item's image (`P18`), when Commons says it is free to show.
async fn image(egress: &Egress, mbid: &str, qid: &str, width: u32) -> Found {
    let files = match wikidata::files(egress, qid, "P18").await {
        Ok(files) => files,
        Err(error) => {
            unread(mbid, "wikidata_image", &error);
            return Found::Unread;
        }
    };
    let Some(name) = files.first().and_then(|f| crate::wikimedia::file_name(f)) else {
        return Found::Missing;
    };
    let fetched = match egress
        .discover(
            &crate::wikimedia::file_query(&name, width),
            &json_options(),
            CACHE_SECS,
        )
        .await
    {
        Ok(fetched) if fetched.status == 200 => fetched,
        Ok(fetched) if matches!(fetched.status, 404 | 410) => return Found::Missing,
        // A wait, or a refusal of this client: no answer about the file.
        Ok(fetched) => {
            tracing::info!(
                mbid,
                status = fetched.status,
                "music enrichment: Commons did not answer"
            );
            return Found::Unread;
        }
        Err(error) => {
            unread(mbid, "commons_file", &error);
            return Found::Unread;
        }
    };
    // The author is HTML: read off the runtime's threads, as every HTML parse is.
    let body = fetched.body;
    let file = name.clone();
    // `None`: not JSON; `Some(None)`: an answer about another file.
    let check = crate::resolver::html::blocking(move || {
        serde_json::from_slice::<Value>(&body)
            .ok()
            .map(|v| crate::wikimedia::free_file(&v, &file))
    })
    .await
    .unwrap_or_else(|error| {
        unread(mbid, "commons_parse", &error);
        None
    });
    match check {
        Some(Some(crate::wikimedia::FileCheck::Free(file))) => {
            let credit = Credit {
                field: CreditField::Image,
                provider: CreditProvider::WikimediaCommons,
                source_url: file.page,
                author: file.author,
                license: file.license.id,
                license_url: file.license.url,
                lang: None,
                excerpted: false,
                modified: false,
                subject: None,
            };
            match credit.bounded() {
                Some(credit) => Found::Value(file.image, credit),
                None => Found::Missing,
            }
        }
        Some(Some(crate::wikimedia::FileCheck::Refused(reason))) => {
            tracing::debug!(mbid, reason, "music enrichment: Commons file refused");
            Found::Missing
        }
        Some(Some(crate::wikimedia::FileCheck::Missing)) => Found::Missing,
        // Nothing verifies the file: no image.
        Some(None) => {
            tracing::info!(mbid, "music enrichment: Commons answered for another file");
            Found::Missing
        }
        None => {
            tracing::info!(mbid, "music enrichment: Commons' answer is not JSON");
            Found::Unread
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_text_is_cut_at_a_sentence_or_a_word() {
        assert_eq!(excerpt("Short.", 2048), ("Short.".into(), false));
        let text = format!("{} Second sentence goes past.", "a".repeat(30));
        let (cut, excerpted) = excerpt(&text, 40);
        assert!(excerpted);
        assert_eq!(cut, format!("{} Second…", "a".repeat(30)));
        let text = format!("{}. {}", "First sentence".repeat(2), "word ".repeat(20));
        let (cut, _) = excerpt(&text, 40);
        assert_eq!(cut, format!("{}.", "First sentence".repeat(2)));
        // Never more than the bound, never a split character.
        let text = "é".repeat(3000);
        let (cut, excerpted) = excerpt(&text, 2048);
        assert!(excerpted && cut.len() <= 2048 && cut.ends_with('…'));
        // Real text: the bound holds for every cut.
        let text = "Miles Dewey Davis III was an American trumpeter, bandleader, and composer. "
            .repeat(40);
        for max in [100, 1000, 2048] {
            let (cut, _) = excerpt(&text, max);
            assert!(cut.len() <= max, "{max}");
            assert!(cut.ends_with('.'), "{cut}");
        }
    }
    #[test]
    fn only_a_standard_article_has_text() {
        let standard = serde_json::json!({"type": "standard", "titles": {"canonical": "X"}, "extract": " Text. "});
        assert_eq!(summary_text(&standard, "X").as_deref(), Some("Text."));
        assert_eq!(summary_text(&standard, "Y"), None, "a redirect's target");
        for v in [
            serde_json::json!({"type": "disambiguation", "titles": {"canonical": "X"}, "extract": "X may refer to:"}),
            serde_json::json!({"type": "standard", "titles": {"canonical": "X"}, "extract": " "}),
            serde_json::json!({"type": "standard", "extract": "Text."}),
            serde_json::json!({"type": "no-extract"}),
        ] {
            assert_eq!(summary_text(&v, "X"), None, "{v}");
        }
    }
    #[test]
    fn only_one_own_item_counts() {
        let rel = |url: &str| musicbrainz::UrlRelation {
            kind: "wikidata".into(),
            url: url.into(),
        };
        assert_eq!(
            own_item(&[rel("https://www.wikidata.org/wiki/Q93341")]),
            Own::Item("Q93341".into())
        );
        assert_eq!(
            own_item(&[
                rel("https://www.wikidata.org/wiki/Q1"),
                rel("https://www.wikidata.org/wiki/Q2")
            ]),
            Own::Unclear
        );
        assert_eq!(
            own_item(&[rel("https://www.wikidata.org/wiki/Property:P18")]),
            Own::Unclear
        );
        let other = musicbrainz::UrlRelation {
            kind: "other databases".into(),
            url: "https://www.wikidata.org/wiki/Q1".into(),
        };
        assert_eq!(own_item(&[other]), Own::None);
    }
    /// A recording whose own relations name two items takes nothing, not its work's
    /// description: nothing is asked.
    #[tokio::test]
    async fn a_recording_with_two_own_items_takes_nothing() {
        let rel = |url: &str| musicbrainz::UrlRelation {
            kind: "wikidata".into(),
            url: url.into(),
        };
        let recording = musicbrainz::Entity {
            kind: MbKind::Recording,
            mbid: "833f00e1-781f-4edd-90e4-e52712618862".into(),
            title: Some("Get Lucky".into()),
            urls: vec![
                rel("https://www.wikidata.org/wiki/Q1"),
                rel("https://www.wikidata.org/wiki/Q2"),
            ],
            isrcs: vec![],
            barcode: None,
            release_group: None,
            artist: None,
            length_ms: None,
            artist_type: None,
        };
        let (egress, recorded) = crate::sources::fixture::egress(vec![]);
        assert_eq!(
            enrich(&egress, musicbrainz::BASE, &recording, 640).await,
            Enrichment::missing()
        );
        assert_eq!(recorded.asked(), Vec::<String>::new());
    }
}
