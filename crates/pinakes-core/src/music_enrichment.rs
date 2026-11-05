//! A MusicBrainz card's image and description, from the entity's Wikidata item: its Wikipedia
//! article's summary or the item's English description, and its image on Wikimedia Commons, each
//! with its credit (`model::Credit`).
//!
//! Only exact identifiers lead there, never a name. The item is the one the entity's own
//! `wikidata` URL relation names; without such a relation, the one item that carries the
//! entity's MBID (an artist's P434, a release group's P436, a release's P5813, a recording's
//! P4404), found by Wikidata's search and read back to check the claim (`wikidata::item_with`);
//! for an artist no item carries the MBID of, the one item its Discogs artist pages on
//! MusicBrainz (P1953) all name, unless that item carries another artist's MBID. Several items,
//! or relations that name several, give nothing. A release and its release group each use their
//! own item. A recording's item is one recording's (`music_sources::recording_item`), never a
//! song's. A recording without one may take the description of the one work it is a
//! performance of, when that work has its own relation; the credit then names the work
//! (`subject`), and the work's image is never the recording's.
//!
//! The description is, in this order, the first of: the item's English Wikipedia article's
//! summary; the item's English description on Wikidata; the summary of the first article the
//! item has in another Wikipedia of [`OTHER_WIKIPEDIAS`]. Cards have no language negotiation and
//! clients read English, so an English paragraph is best, and a short, exact English line beats a
//! paragraph a reader may not be able to read; another language is better than nothing, and its
//! credit's `lang` lets a client label or hide it. Nothing is translated. A summary is used as
//! plain text, never its thumbnail (an album cover there is usually a non-free file of
//! Wikipedia's own). The image is shown only when Commons says the file is free to show with its
//! credit (`wikimedia::free_file`).
//!
//! Each field ends in one of three ways: a value with its credit; missing (no item, an article
//! that is not there (404, 410), a sitelink to a redirect, a disambiguation page, no image, a
//! refused file), which leaves the card as MusicBrainz alone makes it; or unread (a timeout, a
//! 429, a 5xx, Wikidata's `maxlag`, any other status), which keeps what the stored card holds
//! (`ResourceMeta::keep_enriched`). A missing text gives way to the next in the order; an unread
//! one ends the description unread, so a busy Wikipedia never puts a shorter text in place of a
//! stored article's. Every request goes through the egress, whose shared flights and cache make
//! concurrent cards of one entity ask once.
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
/// The licence of Wikidata's data.
const WIKIDATA_LICENSE: (&str, &str) = (
    "CC0-1.0",
    "https://creativecommons.org/publicdomain/zero/1.0/",
);
/// The Wikipedias whose summary describes an item with neither an English article nor an
/// English description: the first of them the item has an article in, in this order.
pub const OTHER_WIKIPEDIAS: [&str; 10] =
    ["de", "fr", "es", "it", "nl", "pt", "sv", "pl", "ja", "ru"];
/// At most this many of an artist's Discogs artist pages are looked up on Wikidata.
pub const DISCOGS_LOOKUPS: usize = 2;
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

/// The Wikidata property whose value is a MusicBrainz ID of `kind`.
pub fn mbid_property(kind: MbKind) -> &'static str {
    match kind {
        MbKind::Artist => "P434",
        MbKind::ReleaseGroup => "P436",
        MbKind::Release => "P5813",
        MbKind::Recording => "P4404",
    }
}
/// The one item that names `entity` when its own relations name none: the item carrying its
/// MBID, or, for an artist no item carries the MBID of, the item its Discogs artist pages name.
/// An error is a transient failure (a busy Wikidata).
async fn named_item(
    egress: &Egress,
    entity: &musicbrainz::Entity,
) -> anyhow::Result<Option<String>> {
    match wikidata::item_with(egress, mbid_property(entity.kind), &entity.mbid).await? {
        // A song's item (a work) may carry its recordings' IDs: it is no recording's own.
        wikidata::Found::One(item)
            if entity.kind == MbKind::Recording && !crate::music_sources::recording_item(&item) =>
        {
            Ok(None)
        }
        wikidata::Found::One(item) => Ok(Some(item.qid)),
        wikidata::Found::Ambiguous(_) => Ok(None),
        wikidata::Found::None if entity.kind == MbKind::Artist => {
            discogs_item(egress, entity).await
        }
        wikidata::Found::None => Ok(None),
    }
}
/// The item an artist's Discogs artist pages (its current `discogs` relations on MusicBrainz, at
/// most [`DISCOGS_LOOKUPS`]) name: every page that names an item names the same one, alone, and
/// that item carries no MusicBrainz artist ID or carries this artist's (one with another
/// artist's is someone else).
async fn discogs_item(
    egress: &Egress,
    entity: &musicbrainz::Entity,
) -> anyhow::Result<Option<String>> {
    let mut ids: Vec<u64> = vec![];
    for relation in entity.urls.iter().filter(|r| r.kind == "discogs") {
        if let Some((crate::music::discogs::Kind::Artist, id)) =
            crate::music::discogs::of_uri(&relation.url)
        {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut named: Option<Box<wikidata::Item>> = None;
    for id in ids.into_iter().take(DISCOGS_LOOKUPS) {
        match wikidata::item_with(egress, "P1953", &id.to_string()).await? {
            wikidata::Found::None => {}
            wikidata::Found::Ambiguous(_) => return Ok(None),
            wikidata::Found::One(item) => match &named {
                Some(other) if other.qid != item.qid => return Ok(None),
                _ => named = Some(item),
            },
        }
    }
    Ok(named
        .filter(|item| {
            let artists = item.id(mbid_property(MbKind::Artist));
            artists.is_empty() || artists.contains(&entity.mbid)
        })
        .map(|item| item.qid))
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
        // An entity whose own relations are unclear is not looked up otherwise.
        Own::Unclear => return Enrichment::missing(),
        Own::None => {}
    }
    match named_item(egress, entity).await {
        Ok(Some(qid)) => return item(egress, &entity.mbid, &qid, None, width).await,
        Ok(None) => {}
        Err(error) => {
            unread(&entity.mbid, "wikidata_search", &error);
            return Enrichment::unread();
        }
    }
    if entity.kind != MbKind::Recording {
        return Enrichment::missing();
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

/// The item's description, the first of its English Wikipedia article's summary, its English
/// description, and its first article in [`OTHER_WIKIPEDIAS`]: a missing one gives way to the
/// next, an unread one leaves the description unread.
async fn description(egress: &Egress, mbid: &str, qid: &str, subject: Option<String>) -> Found {
    let pages = match wikidata::pages(egress, qid).await {
        Ok(Some(pages)) => pages,
        Ok(None) => return Found::Missing,
        Err(error) => {
            unread(mbid, "wikidata_pages", &error);
            return Found::Unread;
        }
    };
    if let Some(page) = pages.sitelinks.get("enwiki") {
        match summary(egress, mbid, page, "en", subject.clone()).await {
            Found::Missing => {}
            found => return found,
        }
    }
    if let Some(found) = pages
        .description
        .as_deref()
        .and_then(|text| item_description(qid, text, subject.clone()))
    {
        return found;
    }
    let other = OTHER_WIKIPEDIAS
        .iter()
        .find_map(|lang| Some((*lang, pages.sitelinks.get(&format!("{lang}wiki"))?)));
    match other {
        Some((lang, page)) => summary(egress, mbid, page, lang, subject).await,
        None => Found::Missing,
    }
}

/// An item's English description on Wikidata as it is (trimmed), credited to Wikidata under
/// CC0; `None` for an empty one.
fn item_description(qid: &str, text: &str, subject: Option<String>) -> Option<Found> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (text, excerpted) = excerpt(text, crate::model::DESCRIPTION_CAP);
    let credit = Credit {
        field: CreditField::Description,
        provider: CreditProvider::Wikidata,
        source_url: format!("https://www.wikidata.org/wiki/{qid}"),
        author: None,
        license: WIKIDATA_LICENSE.0.into(),
        license_url: Some(WIKIDATA_LICENSE.1.into()),
        lang: Some("en".into()),
        excerpted,
        modified: false,
        subject,
    };
    credit.bounded().map(|credit| Found::Value(text, credit))
}

/// The summary of the article `page` (an item's sitelink) in the `lang` Wikipedia.
async fn summary(
    egress: &Egress,
    mbid: &str,
    page: &str,
    lang: &str,
    subject: Option<String>,
) -> Found {
    let Some(article) = crate::wikipedia::parse(page).filter(|a| a.lang == lang) else {
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
    fn json(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }
    /// An item's pages answer: sitelinks by language, and its English description.
    fn pages(qid: &str, langs: &[&str], description: Option<&str>) -> Vec<u8> {
        let sitelinks: serde_json::Map<String, Value> = langs
            .iter()
            .map(|l| {
                (
                    format!("{l}wiki"),
                    serde_json::json!({"site": format!("{l}wiki"), "title": "X", "badges": [], "url": format!("https://{l}.wikipedia.org/wiki/X")}),
                )
            })
            .collect();
        let descriptions = match description {
            Some(d) => serde_json::json!({"en": {"language": "en", "value": d}}),
            None => serde_json::json!({}),
        };
        json(
            serde_json::json!({"entities": {qid: {"type": "item", "id": qid, "sitelinks": sitelinks, "descriptions": descriptions}}}),
        )
    }
    fn article(lang: &str) -> Vec<u8> {
        json(
            serde_json::json!({"type": "standard", "titles": {"canonical": "X"}, "extract": format!("Text in {lang}.")}),
        )
    }
    /// The description is the English article's summary, else the item's English description,
    /// else the first other Wikipedia's summary in the fixed order; a missing text gives way to
    /// the next, an unread one (here a 503) ends it unread, and nothing else is asked.
    #[tokio::test(start_paused = true)]
    async fn the_text_comes_in_a_fixed_order() {
        let pages_url = wikidata::pages_lookup("Q1");
        let summary =
            |lang: &str| format!("https://{lang}.wikipedia.org/api/rest_v1/page/summary/X");
        let found = |routes: Vec<(String, u16, Vec<u8>)>| async move {
            let routes: Vec<(&str, u16, &'static str, Vec<u8>)> = routes
                .iter()
                .map(|(u, s, b)| (u.as_str(), *s, "application/json", b.clone()))
                .collect();
            let (egress, recorded) = crate::sources::fixture::egress(routes);
            let found = description(&egress, "m", "Q1", None).await;
            (found, recorded.asked())
        };
        let text = |f: &Found| match f {
            Found::Value(text, credit) => {
                Some((text.clone(), credit.provider, credit.lang.clone().unwrap()))
            }
            _ => None,
        };
        let line = Some("a line");
        // 1. The English article.
        let (f, asked) = found(vec![
            (pages_url.clone(), 200, pages("Q1", &["en", "de"], line)),
            (summary("en"), 200, article("en")),
        ])
        .await;
        assert_eq!(
            text(&f),
            Some(("Text in en.".into(), CreditProvider::Wikipedia, "en".into()))
        );
        assert_eq!(asked, [pages_url.clone(), summary("en")]);
        // 2. No English article (404): the item's English description, whole, CC0.
        let (f, asked) = found(vec![
            (pages_url.clone(), 200, pages("Q1", &["en", "de"], line)),
            (summary("en"), 404, b"{}".to_vec()),
        ])
        .await;
        let Found::Value(value, credit) = &f else {
            panic!("{f:?}")
        };
        assert_eq!(value, "a line");
        assert_eq!(
            serde_json::to_value(credit).unwrap(),
            serde_json::json!({
                "field": "description", "provider": "wikidata",
                "source_url": "https://www.wikidata.org/wiki/Q1", "author": null,
                "license": "CC0-1.0", "license_url": "https://creativecommons.org/publicdomain/zero/1.0/",
                "lang": "en", "excerpted": false, "modified": false, "subject": null,
            })
        );
        assert_eq!(asked, [pages_url.clone(), summary("en")]);
        // An unread English article is no reason to show a shorter text.
        let (f, _) = found(vec![
            (pages_url.clone(), 200, pages("Q1", &["en", "de"], line)),
            (summary("en"), 503, b"{}".to_vec()),
        ])
        .await;
        assert_eq!(f, Found::Unread);
        // 3. Neither: the first other Wikipedia in the order (French before Dutch), alone.
        let (f, asked) = found(vec![
            (
                pages_url.clone(),
                200,
                pages("Q1", &["nl", "fr", "zh"], Some("  ")),
            ),
            (summary("fr"), 200, article("fr")),
            (summary("nl"), 200, article("nl")),
        ])
        .await;
        assert_eq!(
            text(&f),
            Some(("Text in fr.".into(), CreditProvider::Wikipedia, "fr".into()))
        );
        assert_eq!(asked, [pages_url.clone(), summary("fr")]);
        let (f, _) = found(vec![
            (pages_url.clone(), 200, pages("Q1", &["nl"], None)),
            (summary("nl"), 503, b"{}".to_vec()),
        ])
        .await;
        assert_eq!(f, Found::Unread);
        // Only a Wikipedia outside the order: nothing, and nothing else asked.
        let (f, asked) = found(vec![(pages_url.clone(), 200, pages("Q1", &["zh"], None))]).await;
        assert_eq!(f, Found::Missing);
        assert_eq!(asked, std::slice::from_ref(&pages_url));
        // A busy Wikidata: unread.
        let (f, _) = found(vec![(pages_url.clone(), 503, b"{}".to_vec())]).await;
        assert_eq!(f, Found::Unread);
        assert_eq!(OTHER_WIKIPEDIAS[..5], ["de", "fr", "es", "it", "nl"]);
    }
    /// A release group, release or recording without its own item is looked up by its own
    /// MBID's property alone, never by a Discogs page; a recording's ID on a song's item (one with
    /// a work ID) names no item, and the recording goes on to its works.
    #[tokio::test(start_paused = true)]
    async fn the_item_is_looked_up_by_the_entitys_own_id() {
        let mbid = "8e8a594f-2175-38c7-a871-abb68ec363e7";
        let entity = |kind: MbKind| musicbrainz::Entity {
            kind,
            mbid: mbid.into(),
            title: None,
            urls: vec![musicbrainz::UrlRelation {
                kind: "discogs".into(),
                url: "https://www.discogs.com/artist/1".into(),
            }],
            isrcs: vec![],
            barcode: None,
            release_group: None,
            artist: None,
            length_ms: None,
            artist_type: None,
            about: Default::default(),
        };
        let none =
            json(serde_json::json!({"query": {"searchinfo": {"totalhits": 0}, "search": []}}));
        for (kind, property) in [
            (MbKind::ReleaseGroup, "P436"),
            (MbKind::Release, "P5813"),
            (MbKind::Artist, "P434"),
        ] {
            assert_eq!(mbid_property(kind), property);
            let search = wikidata::search_lookup(property, mbid).unwrap();
            let discogs = wikidata::search_lookup("P1953", "1").unwrap();
            let (egress, recorded) = crate::sources::fixture::egress(vec![
                (search.as_str(), 200, "application/json", none.clone()),
                (discogs.as_str(), 200, "application/json", none.clone()),
            ]);
            let found = enrich(&egress, musicbrainz::BASE, &entity(kind), 640).await;
            assert_eq!(found, Enrichment::missing());
            let mut expected = vec![search.clone()];
            if kind == MbKind::Artist {
                expected.push(discogs.clone());
            }
            assert_eq!(recorded.asked(), expected, "{kind:?}");
        }
        // A song's item carrying the recording's ID.
        let search = wikidata::search_lookup("P4404", mbid).unwrap();
        let read = wikidata::entities_lookup(&["Q2".into()]);
        let works = musicbrainz::works_lookup(musicbrainz::BASE, mbid);
        let claim = |v: &str| serde_json::json!([{"rank": "normal", "mainsnak": {"snaktype": "value", "datatype": "external-id", "datavalue": {"value": v}}}]);
        let song = json(
            serde_json::json!({"entities": {"Q2": {"type": "item", "id": "Q2", "claims": {
            "P4404": claim(mbid), "P435": claim("b11bf816-1844-47e5-9075-9239b7609c6a")}}}}),
        );
        let hit = json(
            serde_json::json!({"query": {"searchinfo": {"totalhits": 1}, "search": [{"title": "Q2"}]}}),
        );
        let (egress, recorded) = crate::sources::fixture::egress(vec![
            (search.as_str(), 200, "application/json", hit.clone()),
            (read.as_str(), 200, "application/json", song.clone()),
        ]);
        let found = enrich(&egress, musicbrainz::BASE, &entity(MbKind::Recording), 640).await;
        assert_eq!(found, Enrichment::missing());
        assert_eq!(recorded.asked(), [search.clone(), read.clone(), works]);
        // The same item without the work ID is the recording's own.
        let recording = json(
            serde_json::json!({"entities": {"Q2": {"type": "item", "id": "Q2", "claims": {
            "P4404": claim(mbid)}}}}),
        );
        let (egress, recorded) = crate::sources::fixture::egress(vec![
            (search.as_str(), 200, "application/json", hit),
            (read.as_str(), 200, "application/json", recording),
        ]);
        enrich(&egress, musicbrainz::BASE, &entity(MbKind::Recording), 640).await;
        assert!(recorded.asked().contains(&wikidata::pages_lookup("Q2")));
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
            about: Default::default(),
        };
        let (egress, recorded) = crate::sources::fixture::egress(vec![]);
        assert_eq!(
            enrich(&egress, musicbrainz::BASE, &recording, 640).await,
            Enrichment::missing()
        );
        assert_eq!(recorded.asked(), Vec::<String>::new());
    }
}
