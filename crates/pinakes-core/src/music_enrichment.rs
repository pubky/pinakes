//! A MusicBrainz card's image and description, from the entity's Wikidata item: its Wikipedia
//! article's summary or the item's English description, and its image on Wikimedia Commons, each
//! with its credit (`model::Credit`). A release or release group with no free Commons file takes
//! its front cover from the Cover Art Archive, by its MBID ([`cover`]).
//!
//! Only exact identifiers lead there, never a name. The item is the one the entity's own
//! `wikidata` URL relation names; without such a relation, the one item that carries the
//! entity's MBID (an artist's P434, a release group's P436, a release's P5813, a recording's
//! P4404), found by Wikidata's search and read back to check the claim (`wikidata::item_with`);
//! for an artist no item carries the MBID of, the one item its Discogs artist pages on
//! MusicBrainz (P1953) all name. Several items, or relations that name several, give nothing.
//! However it was found, by the relation too, the item is read back and is the entity's only when
//! it is ([`is_item_of`]): one classifier tells what an item is the item of by the MusicBrainz
//! and Discogs IDs it carries ([`ItemKind::of`]; never a Wikimedia page's, such as a disambiguation
//! page's or a list's, [`NOT_AN_ENTITY`]), and one table says which kinds each card takes
//! ([`TAKES`]): an artist an artist's item, a release group a release group's (or one that is a
//! work's too: a single's song's), a release a release's, a recording one recording's, never a
//! song's, each naming no other entity of its level. Otherwise the card takes nothing from it, and no other item is
//! looked for. A release and its release group each use their own item.
//! An item too large to read verifies nothing. A recording may also take the description of the
//! one work it is a performance of, when that work has its own relation to an item that is the
//! work's by the same table (a song's, or its single's); the credit then names the work
//! (`subject`), and the work's image is never the recording's. An item whose Wikipedia article is a disambiguation page is no entity's
//! either: nothing of it is taken.
//!
//! The description is, in this order, the first of: the item's English Wikipedia article's summary;
//! the item's English description on Wikidata; the summary of the first article the item has in
//! another Wikipedia of [`OTHER_WIKIPEDIAS`]. Cards have no language negotiation and clients read
//! English, so an English paragraph is best, and a short, exact English line beats a paragraph a
//! reader may not be able to read; another language is better than nothing, and its credit's `lang`
//! lets a client label or hide it. Nothing is translated. For a recording, its work's English
//! article comes right after its own item's, and the work's English description and other Wikipedia
//! after its own item's: a song's article says more about a recording than a one-line description
//! of the recording does. A summary is used as plain text, never its thumbnail (an album cover
//! there is usually a non-free file of Wikipedia's own). The image is shown only when Commons says
//! the file is free to show with its credit (`wikimedia::free_file`).
//!
//! Each field ends in one of three ways: a value with its credit; missing (no item, an item, an
//! entity or an article that is not there (404, 410), a sitelink to a redirect, a disambiguation page, no image, a
//! refused file), which leaves the card as MusicBrainz alone makes it; or unread (a timeout, a
//! 429, a 5xx, Wikidata's `maxlag`, any other status), which keeps what the stored card holds
//! (`ResourceMeta::keep_enriched`). A missing text gives way to the next in the order; an unread
//! one ends the description unread, so a busy Wikipedia never puts a shorter text in place of a
//! stored article's. Every request goes through the egress, whose shared flights and cache make
//! concurrent cards of one entity ask once.
pub use crate::wikimedia::Found;
use crate::{
    hubs::{
        musicbrainz::{self, own_item, OwnItem as Own},
        wikidata, MbKind,
    },
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
    /// Writes the found values and their credits into `doc` (a value that is or holds a link
    /// carrying a credential, `capability::contains_credential`, leaves the field as MusicBrainz
    /// makes it: a description is free text). An unread field is listed in
    /// `unread_enriched`, and the card is read again early, `retry_secs` from now at the soonest
    /// (`ResourceMeta::keep_enriched` moves that later by how old its stored reading is).
    pub fn apply(self, doc: &mut ResourceMeta, retry_secs: u64) {
        for (field, found) in [
            (CreditField::Image, self.image),
            (CreditField::Description, self.description),
        ] {
            match found {
                // A value the store would drop (`capability::strip_document`) is not taken: its
                // credit would stay without it. Nor is a text with such a link inside, which the
                // store would keep.
                Found::Value(value, _) if crate::capability::contains_credential(&value) => {}
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

/// The Wikidata property whose value is a MusicBrainz ID of `kind`.
pub fn mbid_property(kind: MbKind) -> &'static str {
    match kind {
        MbKind::Artist => "P434",
        MbKind::ReleaseGroup => "P436",
        MbKind::Release => "P5813",
        MbKind::Recording => "P4404",
    }
}
/// The classes (P31) of an item that is a Wikimedia page's, never an entity's: a disambiguation
/// page (a human name's too), a list article, a category (a disambiguation category too), a
/// template.
pub const NOT_AN_ENTITY: [&str; 6] = [
    "Q4167410",
    "Q22808320",
    "Q13406463",
    "Q4167836",
    "Q15407973",
    "Q11266439",
];
/// The classes (P31) of a song's item, which Wikidata often makes its single's too: a song, a
/// single, a musical work (both classes).
pub const SONG_CLASSES: [&str; 4] = ["Q7366", "Q134556", "Q105543609", "Q2188189"];
/// What a Wikidata item is the item of ([`ItemKind::of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A Wikimedia page's ([`NOT_AN_ENTITY`]), whatever it carries.
    Page,
    /// No identifier of any kind below: Wikidata may not name yet what MusicBrainz relates to it.
    Bare,
    /// An artist's: a MusicBrainz (P434) or Discogs (P1953) artist ID, and nothing else.
    Artist,
    /// A release group's: a MusicBrainz release group (P436) or Discogs master (P1954) ID, maybe
    /// with its releases' and recordings' IDs.
    ReleaseGroup,
    /// A release group's that is a work's too, by a work ID (P435) or a song's class
    /// ([`SONG_CLASSES`]): a song's item that is its single's, a rock opera's that is its album's.
    WorkAndGroup,
    /// A release's: a release ID (P5813), maybe with its recordings', and no release group's.
    Release,
    /// A recording's: a recording ID (P4404), of no song's class.
    Recording,
    /// A work's: a work ID (P435), maybe with its recordings'; or a song's class with its
    /// recordings' IDs and no work ID yet.
    Work,
    /// An artist's ID with another kind's: no one's.
    Mixed,
}
impl ItemKind {
    /// The kind of `item`, read back with its claims, by the identifiers it carries.
    pub fn of(item: &wikidata::Item) -> Self {
        let class = |classes: &[&str]| {
            item.instance_of
                .iter()
                .any(|class| classes.contains(&class.as_str()))
        };
        if class(&NOT_AN_ENTITY) {
            return Self::Page;
        }
        let has = |properties: &[&str]| properties.iter().any(|p| !item.id(p).is_empty());
        let artist = has(&["P434", "P1953"]);
        let others = has(&["P436", "P1954", "P5813", "P4404", "P435"]);
        let (group, work, song) = (
            has(&["P436", "P1954"]),
            has(&["P435"]),
            class(&SONG_CLASSES),
        );
        match () {
            _ if artist && others => Self::Mixed,
            _ if artist => Self::Artist,
            _ if group && (work || song) => Self::WorkAndGroup,
            _ if group => Self::ReleaseGroup,
            _ if work => Self::Work,
            _ if has(&["P5813"]) => Self::Release,
            _ if has(&["P4404"]) && song => Self::Work,
            _ if has(&["P4404"]) => Self::Recording,
            _ => Self::Bare,
        }
    }
    /// The kind of item a MusicBrainz entity of `kind` has.
    pub fn of_entity(kind: MbKind) -> Self {
        match kind {
            MbKind::Artist => Self::Artist,
            MbKind::ReleaseGroup => Self::ReleaseGroup,
            MbKind::Release => Self::Release,
            MbKind::Recording => Self::Recording,
        }
    }
}
/// How an item names the entity whose item it is, by that entity's own property.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Names {
    /// This one ID, alone.
    One,
    /// This one ID alone, or none of its level yet.
    OneOrNone,
    /// This ID among others (an artist's duplicates), or none yet.
    AmongOrNone,
}
/// Which kinds of item each subject takes, and how the item names it (by `property`). A
/// recording's item names it, and is never a song's (a song's item may carry its recordings'
/// IDs, and a recording's relation often names it); a release's is no album's (the release
/// group's: a release and its release group each use their own item); a release group's may be
/// a work's too (its single's song's, a rock opera's); a work's (a recording's song) may be its
/// single's item, never an album's, a recording's, a release's or an artist's. Every subject but a recording takes an item with no
/// identifier yet ([`ItemKind::Bare`]): Wikidata lags behind MusicBrainz.
const TAKES: [(ItemKind, &str, Names, &[ItemKind]); 5] = [
    (
        ItemKind::Artist,
        "P434",
        Names::AmongOrNone,
        &[ItemKind::Artist, ItemKind::Bare],
    ),
    (
        ItemKind::ReleaseGroup,
        "P436",
        Names::OneOrNone,
        &[
            ItemKind::ReleaseGroup,
            ItemKind::WorkAndGroup,
            ItemKind::Bare,
        ],
    ),
    (
        ItemKind::Release,
        "P5813",
        Names::OneOrNone,
        &[ItemKind::Release, ItemKind::Bare],
    ),
    (
        ItemKind::Recording,
        "P4404",
        Names::One,
        &[ItemKind::Recording],
    ),
    (
        ItemKind::Work,
        "P435",
        Names::OneOrNone,
        &[ItemKind::Work, ItemKind::WorkAndGroup, ItemKind::Bare],
    ),
];
/// Whether `item`, read back with its claims, is the item of the `subject` (an entity's kind, or
/// a work) with MusicBrainz ID `mbid`, however it was found (the entity's own relation, its MBID,
/// its Discogs pages, a recording's work's relation): of a kind [`TAKES`] lets the subject take,
/// naming it as it says. An item found by an MBID names it; one a relation names may not yet.
pub fn is_item_of(subject: ItemKind, mbid: &str, item: &wikidata::Item) -> bool {
    let Some((_, property, names, kinds)) = TAKES.iter().find(|(s, ..)| *s == subject) else {
        return false;
    };
    if !kinds.contains(&ItemKind::of(item)) {
        return false;
    }
    let ids = item.id(property);
    match names {
        Names::One => ids == [mbid],
        Names::OneOrNone => ids.is_empty() || ids == [mbid],
        Names::AmongOrNone => ids.is_empty() || ids.iter().any(|id| id == mbid),
    }
}
/// Whether `item` is `entity`'s own ([`is_item_of`]).
fn is_own(entity: &musicbrainz::Entity, item: &wikidata::Item) -> bool {
    is_item_of(ItemKind::of_entity(entity.kind), &entity.mbid, item)
}
/// A hub's answer where a 410 (`hubs::Gone`: the item or entity is gone for good) is `missing`,
/// as a 404 is: nothing to read, no transient failure to read again.
fn gone_is<T>(answer: anyhow::Result<T>, missing: T) -> anyhow::Result<T> {
    match answer {
        Err(error) if error.downcast_ref::<crate::hubs::Gone>().is_some() => Ok(missing),
        answer => answer,
    }
}
/// Item `qid` read back with its claims (`wikidata::items`); `None` when it is unknown, or too
/// large to read and so can't be verified. An error is a transient failure.
async fn read_back(egress: &Egress, qid: &str) -> anyhow::Result<Option<wikidata::Item>> {
    match gone_is(wikidata::items(egress, &[qid.to_owned()]).await, vec![]) {
        Ok(items) => Ok(items.into_iter().next()),
        Err(error) if crate::resolver::egress::over_cap(&error) => Ok(None),
        Err(error) => Err(error),
    }
}
/// `wikidata::item_with`, where an item too large to read can't be verified: `None` then.
async fn verified_item(
    egress: &Egress,
    property: &str,
    value: &str,
) -> anyhow::Result<Option<wikidata::Found>> {
    match gone_is(
        wikidata::item_with(egress, property, value).await,
        wikidata::Found::None,
    ) {
        Ok(found) => Ok(Some(found)),
        Err(error) if crate::resolver::egress::over_cap(&error) => Ok(None),
        Err(error) => Err(error),
    }
}
/// The one item that names `entity` when its own relations name none: the item carrying its
/// MBID, or, for an artist no item carries the MBID of, the item its Discogs artist pages name.
/// An error is a transient failure (a busy Wikidata).
async fn named_item(
    egress: &Egress,
    entity: &musicbrainz::Entity,
) -> anyhow::Result<Option<String>> {
    match verified_item(egress, mbid_property(entity.kind), &entity.mbid).await? {
        Some(wikidata::Found::One(item)) => Ok(is_own(entity, &item).then_some(item.qid)),
        Some(wikidata::Found::None) if entity.kind == MbKind::Artist => {
            discogs_item(egress, entity).await
        }
        // Several items, none, or one that can't be read.
        _ => Ok(None),
    }
}
/// The item an artist's Discogs artist pages (its current `discogs` relations on MusicBrainz, at
/// most [`DISCOGS_LOOKUPS`]) name: every page that names an item names the same one, alone, and
/// that item is the artist's own ([`is_own`]: one with another artist's MBID is someone else).
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
        match verified_item(egress, "P1953", &id.to_string()).await? {
            Some(wikidata::Found::None) => {}
            Some(wikidata::Found::One(item)) => match &named {
                Some(other) if other.qid != item.qid => return Ok(None),
                _ => named = Some(item),
            },
            // Several items, or one that can't be read.
            _ => return Ok(None),
        }
    }
    Ok(named
        .filter(|item| is_own(entity, item))
        .map(|item| item.qid))
}

/// The image and description of `entity` (`base`: the MusicBrainz web service), a Commons
/// thumbnail `width` pixels wide. A release or release group with no free Commons file takes
/// its front cover from the Cover Art Archive ([`cover`]); a Commons file that went unread asks
/// for no cover, so a stored image stays.
pub async fn enrich(
    egress: &Egress,
    base: &str,
    entity: &musicbrainz::Entity,
    width: u32,
) -> Enrichment {
    let mut enrichment = wikimedia(egress, base, entity, width).await;
    if enrichment.image == Found::Missing {
        enrichment.image = cover(egress, entity).await;
    }
    enrichment
}

/// The front cover of a release or release group from the Cover Art Archive, by its MBID alone
/// (`hubs::cover_art`), in at most one request: a release's only when MusicBrainz says the
/// archive shows a front cover for it (`About::cover_art_front`), a release group's always (one
/// of its releases' covers). The credit names the release the cover is of (its cover art page on
/// MusicBrainz) and no licence or author: cover art is its label's. A 404 is none; a busy
/// archive, a failed request or an answer that proves nothing (`cover_art::read_front`) leaves
/// the image unread.
async fn cover(egress: &Egress, entity: &musicbrainz::Entity) -> Found {
    let ask = match entity.kind {
        MbKind::Release => entity.about.cover_art_front,
        MbKind::ReleaseGroup => true,
        MbKind::Recording | MbKind::Artist => false,
    };
    if !ask {
        return Found::Missing;
    }
    let front = match crate::hubs::cover_art::front(egress, entity.kind, &entity.mbid).await {
        Ok(Some(front)) => front,
        Ok(None) => return Found::Missing,
        Err(error) => {
            unread(&entity.mbid, "cover_art", &error);
            return Found::Unread;
        }
    };
    let credit = Credit {
        field: CreditField::Image,
        provider: CreditProvider::CoverArtArchive,
        source_url: format!(
            "https://musicbrainz.org/release/{}/cover-art",
            front.release
        ),
        author: None,
        license: String::new(),
        license_url: None,
        lang: None,
        excerpted: false,
        modified: false,
        subject: None,
    };
    match credit.bounded() {
        Some(credit) => Found::Value(front.image, credit),
        None => Found::Missing,
    }
}

/// The image and description `entity` takes from its Wikidata item: the item's Commons file,
/// and its or its work's text ([`description`]).
async fn wikimedia(
    egress: &Egress,
    base: &str,
    entity: &musicbrainz::Entity,
    width: u32,
) -> Enrichment {
    let qid = match own_item(&entity.urls) {
        // The item the entity's own relation names is read back like a found one: a recording's
        // relation may name its song (a work), a release's its album (the release group's item).
        // Such an item is not the entity's, and none is looked for in its place.
        Own::Item(qid) => match read_back(egress, &qid).await {
            Ok(item) => item
                .filter(|item| is_own(entity, item))
                .map(|item| item.qid),
            Err(error) => {
                unread(&entity.mbid, "wikidata_item", &error);
                return Enrichment::unread();
            }
        },
        // An entity whose own relations are unclear is not looked up otherwise.
        Own::Unclear => return Enrichment::missing(),
        Own::None => match named_item(egress, entity).await {
            Ok(qid) => qid,
            Err(error) => {
                unread(&entity.mbid, "wikidata_search", &error);
                return Enrichment::unread();
            }
        },
    };
    // Only a recording has text without an item of its own: its work's.
    if qid.is_none() && entity.kind != MbKind::Recording {
        return Enrichment::missing();
    }
    let (description, image) =
        tokio::join!(description(egress, base, entity, qid.as_deref()), async {
            match &qid {
                Some(qid) => image(egress, &entity.mbid, qid, width).await,
                // A work's image is never its recording's.
                None => Found::Missing,
            }
        });
    match description {
        Text::Found(description) => Enrichment { image, description },
        // The item's article is a disambiguation page: the item is no entity's, and nothing of
        // it (text, image, another Wikipedia) is the card's; a recording keeps its work's text.
        Text::NotOwn(description) => Enrichment {
            image: Found::Missing,
            description,
        },
    }
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

/// An item a description may come from: its pages, and the subject its credit names (the work,
/// for a recording's work's item).
struct Source {
    qid: String,
    pages: wikidata::Pages,
    subject: Option<String>,
}
/// Item `qid`'s pages; `None` for an unknown item, `Err` when unread.
async fn source(
    egress: &Egress,
    mbid: &str,
    qid: &str,
    subject: Option<String>,
) -> Result<Option<Source>, ()> {
    match gone_is(wikidata::pages(egress, qid).await, None) {
        Ok(pages) => Ok(pages.map(|pages| Source {
            qid: qid.to_owned(),
            pages,
            subject,
        })),
        Err(error) => {
            unread(mbid, "wikidata_pages", &error);
            Err(())
        }
    }
}
/// The item of the one work `recording` is a current performance of, when the work's own
/// relations name one and that item, read back, is the work's ([`is_item_of`] with
/// [`ItemKind::Work`]: a work's or its single's song's item naming no other work, never an
/// album's, a recording's, a release's, an artist's or a Wikimedia page's); `Err` when unread.
async fn work_source(egress: &Egress, base: &str, recording: &str) -> Result<Option<Source>, ()> {
    let works = gone_is(musicbrainz::works(egress, base, recording).await, vec![])
        .map_err(|error| unread(recording, "musicbrainz_works", &error))?;
    let [work] = works.as_slice() else {
        return Ok(None);
    };
    let Own::Item(qid) = own_item(&work.urls) else {
        return Ok(None);
    };
    let item = read_back(egress, &qid)
        .await
        .map_err(|error| unread(recording, "wikidata_work_item", &error))?;
    let Some(item) = item.filter(|item| is_item_of(ItemKind::Work, &work.mbid, item)) else {
        return Ok(None);
    };
    let subject = format!("https://musicbrainz.org/work/{}", work.mbid);
    source(egress, recording, &item.qid, Some(subject)).await
}
/// A text's outcome; `NotOwn` when the entity's own item turned out to be no entity's (its
/// article is a disambiguation page), with what other sources gave (a recording's work's text).
#[derive(Debug, PartialEq)]
enum Text {
    Found(Found),
    NotOwn(Found),
}
/// The summary of `source`'s English Wikipedia article.
async fn english_article(egress: &Egress, mbid: &str, source: &Source) -> Text {
    match source.pages.sitelinks.get("enwiki") {
        Some(page) => summary(egress, mbid, page, "en", source.subject.clone()).await,
        None => Text::Found(Found::Missing),
    }
}

/// The description of `entity`, from its item `qid` and, for a recording, from the one work it
/// is a performance of (credited to the work), the first of: the item's English Wikipedia
/// article's summary; the work's; the item's English description; the work's; the summary of
/// one article in [`OTHER_WIKIPEDIAS`] (the item's first, else the work's). The works are read
/// only when the item has no English article. A missing text gives way to the next, an unread
/// one leaves the description unread. An article that is a disambiguation page makes its item
/// no entity's: the entity's own item then gives nothing ([`Text::NotOwn`]; a recording goes on
/// to its work, as when it has no item), a work's item gives no text.
async fn description(
    egress: &Egress,
    base: &str,
    entity: &musicbrainz::Entity,
    qid: Option<&str>,
) -> Text {
    let mbid = entity.mbid.as_str();
    let mut sources: Vec<Source> = vec![];
    if let Some(qid) = qid {
        match source(egress, mbid, qid, None).await {
            Ok(own) => sources.extend(own),
            Err(()) => return Text::Found(Found::Unread),
        }
    }
    // The own item's English article is a disambiguation page: nothing more of the item.
    let mut not_own = false;
    if let Some(own) = sources.first() {
        match english_article(egress, mbid, own).await {
            Text::Found(Found::Missing) => {}
            Text::NotOwn(_) if entity.kind == MbKind::Recording => {
                not_own = true;
                sources.clear();
            }
            text => return text,
        }
    }
    let outcome = |found: Found| {
        if not_own {
            Text::NotOwn(found)
        } else {
            Text::Found(found)
        }
    };
    if entity.kind == MbKind::Recording {
        match work_source(egress, base, mbid).await {
            Ok(Some(work)) => match english_article(egress, mbid, &work).await {
                Text::Found(Found::Missing) => sources.push(work),
                // The work's item is no work's: nothing of it.
                Text::NotOwn(_) => {}
                Text::Found(found) => return outcome(found),
            },
            Ok(None) => {}
            Err(()) => return outcome(Found::Unread),
        }
    }
    if let Some(found) = sources.iter().find_map(|s| {
        let text = s.pages.description.as_deref()?;
        item_description(&s.qid, text, s.subject.clone())
    }) {
        return outcome(found);
    }
    let other = sources.iter().find_map(|s| {
        OTHER_WIKIPEDIAS
            .iter()
            .find_map(|lang| Some((*lang, s.pages.sitelinks.get(&format!("{lang}wiki"))?, s)))
    });
    match other {
        Some((lang, page, s)) => {
            match summary(egress, mbid, page, lang, s.subject.clone()).await {
                // Another Wikipedia's disambiguation page: a work's gives no text; the entity's
                // own item is no entity's.
                Text::NotOwn(_) if s.subject.is_some() => outcome(Found::Missing),
                Text::NotOwn(_) => Text::NotOwn(Found::Missing),
                Text::Found(found) => outcome(found),
            }
        }
        None => outcome(Found::Missing),
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

/// The summary of the article `page` (an item's sitelink) in the `lang` Wikipedia;
/// [`Text::NotOwn`] when the article is a disambiguation page.
async fn summary(
    egress: &Egress,
    mbid: &str,
    page: &str,
    lang: &str,
    subject: Option<String>,
) -> Text {
    Text::Found(
        match read_summary(egress, mbid, page, lang, subject).await {
            Ok(found) => found,
            Err(Disambiguation) => return Text::NotOwn(Found::Missing),
        },
    )
}
/// The article is a disambiguation page.
struct Disambiguation;
async fn read_summary(
    egress: &Egress,
    mbid: &str,
    page: &str,
    lang: &str,
    subject: Option<String>,
) -> Result<Found, Disambiguation> {
    let Some(article) = crate::wikipedia::parse(page).filter(|a| a.lang == lang) else {
        return Ok(Found::Missing);
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
            return Ok(Found::Unread);
        }
    };
    match fetched.status {
        200 => {}
        // No such article.
        404 | 410 => return Ok(Found::Missing),
        // A wait, or a refusal of this client (a 403 for its User-Agent): no answer about the
        // article.
        status => {
            tracing::info!(mbid, status, "music enrichment: Wikipedia did not answer");
            return Ok(Found::Unread);
        }
    }
    let Ok(v) = serde_json::from_slice::<Value>(&fetched.body) else {
        tracing::info!(mbid, "music enrichment: Wikipedia's summary is not JSON");
        return Ok(Found::Unread);
    };
    if is_disambiguation(&v, &article.title) {
        return Err(Disambiguation);
    }
    let Some(text) = summary_text(&v, &article.title) else {
        return Ok(Found::Missing);
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
    Ok(match credit.bounded() {
        Some(credit) => Found::Value(text, credit),
        None => Found::Missing,
    })
}

/// Whether a summary is of the disambiguation page `title` (with `_` for spaces) itself.
pub fn is_disambiguation(v: &Value, title: &str) -> bool {
    v["type"].as_str() == Some("disambiguation") && v["titles"]["canonical"].as_str() == Some(title)
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

/// The item's image (`P18`), when Commons says it is free to show
/// (`wikimedia::credited_image`).
async fn image(egress: &Egress, mbid: &str, qid: &str, width: u32) -> Found {
    let files = match gone_is(wikidata::files(egress, qid, "P18").await, vec![]) {
        Ok(files) => files,
        Err(error) => {
            unread(mbid, "wikidata_image", &error);
            return Found::Unread;
        }
    };
    let Some(name) = files.first().and_then(|f| crate::wikimedia::file_name(f)) else {
        return Found::Missing;
    };
    crate::wikimedia::credited_image(egress, &name, width, mbid).await
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A found value that is a link carrying a credential is not taken, nor is its credit: the
    /// store would drop the value and keep the credit, or the reverse.
    #[test]
    fn a_value_that_carries_a_credential_is_not_taken() {
        let credit = Credit {
            field: CreditField::Image,
            provider: CreditProvider::WikimediaCommons,
            source_url: "https://commons.wikimedia.org/wiki/File:A.jpg".into(),
            author: Some("A".into()),
            license: "CC-BY-4.0".into(),
            license_url: Some("https://creativecommons.org/licenses/by/4.0/".into()),
            lang: None,
            excerpted: false,
            modified: false,
            subject: None,
        };
        let mut doc = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        Enrichment {
            image: Found::Value(
                "https://upload.wikimedia.org/a.jpg?sig=x".into(),
                credit.clone(),
            ),
            description: Found::Missing,
        }
        .apply(&mut doc, 3600);
        assert_eq!((doc.image, doc.credits), (None, vec![]));
        // A description is free text: a link inside it that carries a credential counts too.
        let mut doc = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        Enrichment {
            image: Found::Missing,
            description: Found::Value(
                "A band. See https://example.org/p?api_key=SECRET for more.".into(),
                Credit {
                    field: CreditField::Description,
                    ..credit.clone()
                },
            ),
        }
        .apply(&mut doc, 3600);
        assert_eq!((doc.description, doc.credits), (None, vec![]));
        let mut doc = ResourceMeta::new("https://musicbrainz.org/artist/x", "musicbrainz", 60);
        Enrichment {
            image: Found::Value("https://upload.wikimedia.org/a.jpg".into(), credit.clone()),
            description: Found::Missing,
        }
        .apply(&mut doc, 3600);
        assert_eq!(doc.credits, [credit]);
    }
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
            let artist = entity_of(MbKind::Artist, "m", &[]);
            let Text::Found(found) =
                description(&egress, musicbrainz::BASE, &artist, Some("Q1")).await
            else {
                panic!("no disambiguation here");
            };
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
            artist_ids: vec![],
            disambiguation: None,
            video: false,
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
            // Without an item, a release group still asks for its cover (here none); a release
            // whose answer says the archive shows no front cover does not.
            if kind == MbKind::ReleaseGroup {
                expected.push(format!(
                    "https://coverartarchive.org/release-group/{mbid}/front-500"
                ));
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
            artist_ids: vec![],
            disambiguation: None,
            video: false,
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

    /// An item is an entity's own at its level, however it was found: a recording's names that
    /// one recording (a song's item, named by the recording's relation, is not); a release's or a
    /// release group's may name none of its level when no other level either (an item its own
    /// relation names, not yet named back); an artist's names no other artist; a Wikimedia page's
    /// item (a disambiguation page) is no one's.
    #[test]
    fn an_item_is_its_entitys_own_at_its_level() {
        let item = |ids: &[(&str, &str)], classes: &[&str]| wikidata::Item {
            qid: "Q1".into(),
            ids: ids.iter().fold(Default::default(), |mut ids, (p, v)| {
                ids.entry(p.to_string())
                    .or_insert_with(Vec::new)
                    .push(v.to_string());
                ids
            }),
            instance_of: classes.iter().map(|c| c.to_string()).collect(),
            ..Default::default()
        };
        let own = |kind: MbKind, item: &wikidata::Item| is_own(&entity_of(kind, "m", &[]), item);
        // A recording.
        assert!(own(MbKind::Recording, &item(&[("P4404", "m")], &[])));
        assert!(!own(MbKind::Recording, &item(&[], &["Q7366"])));
        assert!(!own(
            MbKind::Recording,
            &item(&[("P436", "g")], &["Q134556"])
        ));
        assert!(!own(
            MbKind::Recording,
            &item(&[("P4404", "m"), ("P435", "w")], &[])
        ));
        assert!(!own(
            MbKind::Recording,
            &item(&[("P4404", "m"), ("P4404", "n")], &[])
        ));
        // A release.
        assert!(own(MbKind::Release, &item(&[("P5813", "m")], &[])));
        assert!(own(MbKind::Release, &item(&[], &[])));
        assert!(!own(MbKind::Release, &item(&[("P5813", "n")], &[])));
        assert!(!own(MbKind::Release, &item(&[("P436", "g")], &["Q482994"])));
        // A release group.
        assert!(own(MbKind::ReleaseGroup, &item(&[("P436", "m")], &[])));
        assert!(own(MbKind::ReleaseGroup, &item(&[], &["Q482994"])));
        assert!(!own(MbKind::ReleaseGroup, &item(&[("P436", "n")], &[])));
        assert!(!own(MbKind::ReleaseGroup, &item(&[("P5813", "r")], &[])));
        assert!(!own(MbKind::ReleaseGroup, &item(&[("P4404", "r")], &[])));
        // An artist.
        assert!(own(MbKind::Artist, &item(&[], &["Q5"])));
        assert!(own(
            MbKind::Artist,
            &item(&[("P434", "m"), ("P434", "n")], &[])
        ));
        assert!(!own(MbKind::Artist, &item(&[("P434", "n")], &[])));
        // An item of another kind of entity is no card's: one classifier, one table.
        let album = item(&[("P436", "g"), ("P1954", "5460")], &["Q482994"]);
        let single = item(&[("P436", "g")], &["Q134556"]);
        let recording = item(&[("P4404", "r")], &[]);
        let release = item(&[("P5813", "r")], &[]);
        let artist = item(&[("P434", "a"), ("P1953", "1")], &["Q5"]);
        let work = item(&[("P435", "w"), ("P4404", "r")], &["Q7366"]);
        // A song's item that is its single's too, by its class or its work ID; a rock opera's.
        let song_single = item(&[("P435", "w"), ("P4404", "r"), ("P436", "g")], &["Q7366"]);
        let opera = item(&[("P435", "w"), ("P436", "g")], &["Q482994"]);
        // A song's item with its recordings' IDs and no work ID yet.
        let song = item(&[("P4404", "m")], &["Q2188189"]);
        let mixed = item(&[("P434", "m"), ("P436", "m")], &[]);
        for (it, kind) in [
            (&album, ItemKind::ReleaseGroup),
            (&single, ItemKind::WorkAndGroup),
            (&recording, ItemKind::Recording),
            (&release, ItemKind::Release),
            (&artist, ItemKind::Artist),
            (&work, ItemKind::Work),
            (&song_single, ItemKind::WorkAndGroup),
            (&opera, ItemKind::WorkAndGroup),
            (&song, ItemKind::Work),
            (&mixed, ItemKind::Mixed),
            (&item(&[], &["Q5"]), ItemKind::Bare),
            (&item(&[("P4404", "r")], &["Q4167410"]), ItemKind::Page),
        ] {
            assert_eq!(ItemKind::of(it), kind, "{:?}", it.ids);
        }
        for other in [&album, &single, &recording, &release, &work, &mixed] {
            assert!(!own(MbKind::Artist, other), "{:?}", other.ids);
        }
        for other in [&artist, &recording, &release, &work, &mixed] {
            assert!(!own(MbKind::ReleaseGroup, other), "{:?}", other.ids);
        }
        for other in [&artist, &album, &single, &recording, &work, &mixed] {
            assert!(!own(MbKind::Release, other), "{:?}", other.ids);
        }
        for other in [&artist, &album, &single, &release, &work, &mixed] {
            assert!(!own(MbKind::Recording, other), "{:?}", other.ids);
        }
        assert!(!own(MbKind::Recording, &song));
        // A single's item, a rock opera's, is its release group's and its work's.
        for it in [&single, &song_single, &opera] {
            assert!(is_item_of(ItemKind::ReleaseGroup, "g", it), "{:?}", it.ids);
            assert!(!is_item_of(ItemKind::ReleaseGroup, "h", it), "{:?}", it.ids);
        }
        for it in [&song_single, &opera, &song] {
            assert!(is_item_of(ItemKind::Work, "w", it), "{:?}", it.ids);
        }
        // A single's item is its release group's.
        assert!(is_item_of(ItemKind::ReleaseGroup, "g", &single));
        // A work's item is a work's or its single's song's, naming no other work.
        assert!(is_item_of(ItemKind::Work, "w", &work));
        assert!(is_item_of(ItemKind::Work, "w", &single));
        assert!(is_item_of(ItemKind::Work, "w", &item(&[], &["Q7366"])));
        assert!(!is_item_of(ItemKind::Work, "v", &work));
        for other in [&album, &recording, &release, &artist, &mixed] {
            assert!(!is_item_of(ItemKind::Work, "w", other), "{:?}", other.ids);
        }
        // A Wikimedia page's item is no one's, whatever it names.
        for class in NOT_AN_ENTITY {
            for kind in [MbKind::Artist, MbKind::ReleaseGroup, MbKind::Release] {
                assert!(!own(kind, &item(&[], &[class])), "{class}");
            }
        }
        assert!(!own(
            MbKind::Recording,
            &item(&[("P4404", "m")], &["Q4167410"])
        ));
    }
    fn entity_of(kind: MbKind, mbid: &str, urls: &[(&str, &str)]) -> musicbrainz::Entity {
        musicbrainz::Entity {
            kind,
            mbid: mbid.into(),
            title: None,
            urls: urls
                .iter()
                .map(|(kind, url)| musicbrainz::UrlRelation {
                    kind: (*kind).into(),
                    url: (*url).into(),
                })
                .collect(),
            isrcs: vec![],
            barcode: None,
            release_group: None,
            artist: None,
            artist_ids: vec![],
            disambiguation: None,
            video: false,
            length_ms: None,
            artist_type: None,
            about: Default::default(),
        }
    }
    /// An item's pages answer whose sitelinks all name the article `title`.
    fn titled_pages(qid: &str, title: &str, langs: &[&str], description: Option<&str>) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(&pages(qid, langs, description)).unwrap();
        for link in v["entities"][qid]["sitelinks"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            let lang = link["site"]
                .as_str()
                .unwrap()
                .trim_end_matches("wiki")
                .to_owned();
            link["title"] = title.into();
            link["url"] = format!("https://{lang}.wikipedia.org/wiki/{title}").into();
        }
        json(v)
    }
    fn titled_article(title: &str, lang: &str) -> Vec<u8> {
        json(
            serde_json::json!({"type": "standard", "titles": {"canonical": title}, "extract": format!("{title} in {lang}.")}),
        )
    }
    fn summary_of(title: &str, lang: &str) -> String {
        format!("https://{lang}.wikipedia.org/api/rest_v1/page/summary/{title}")
    }
    const RECORDING: &str = "833f00e1-781f-4edd-90e4-e52712618862";
    const WORK: &str = "b11bf816-1844-47e5-9075-9239b7609c6a";
    /// A works answer: the recording performs one work, whose own relation names `Q2`.
    fn one_work() -> Vec<u8> {
        json(serde_json::json!({"id": RECORDING, "relations": [{
            "target-type": "work", "type": "performance", "ended": false,
            "work": {"id": WORK, "relations": [{
                "target-type": "url", "type": "wikidata", "ended": false,
                "url": {"resource": "https://www.wikidata.org/wiki/Q2"}}]}}]}))
    }
    /// A recording's description is, in this order: its own item's English article, its one
    /// work's English article (naming the work), its item's English description, the work's,
    /// and one other Wikipedia's summary (the recording item's first, else the work's). Its works
    /// are read only when its own item has no English article; an unread step ends the
    /// description unread; the image is the recording item's alone.
    #[tokio::test(start_paused = true)]
    async fn a_recordings_text_comes_from_its_item_then_its_work() {
        let recording = entity_of(
            MbKind::Recording,
            RECORDING,
            &[("wikidata", "https://www.wikidata.org/wiki/Q1")],
        );
        let works = musicbrainz::works_lookup(musicbrainz::BASE, RECORDING);
        let (q1, q2) = (wikidata::pages_lookup("Q1"), wikidata::pages_lookup("Q2"));
        let subject = format!("https://musicbrainz.org/work/{WORK}");
        // Each item read back: the recording's names it, the work's names nothing.
        let read_backs = vec![
            (
                wikidata::entities_lookup(&["Q1".into()]),
                200,
                json(
                    serde_json::json!({"entities": {"Q1": {"type": "item", "id": "Q1", "claims": {
                    "P4404": [{"rank": "normal", "mainsnak": {"snaktype": "value",
                        "datatype": "external-id", "datavalue": {"value": RECORDING}}}]}}}}),
                ),
            ),
            (
                wikidata::entities_lookup(&["Q2".into()]),
                200,
                json(
                    serde_json::json!({"entities": {"Q2": {"type": "item", "id": "Q2", "claims": {}}}}),
                ),
            ),
        ];
        let found = |routes: Vec<(String, u16, Vec<u8>)>| {
            let recording = recording.clone();
            let read_backs = read_backs.clone();
            async move {
                // A route given here takes the place of a read-back's.
                let routes: Vec<(&str, u16, &'static str, Vec<u8>)> = read_backs
                    .iter()
                    .chain(routes.iter())
                    .map(|(u, s, b)| (u.as_str(), *s, "application/json", b.clone()))
                    .collect();
                let (egress, recorded) = crate::sources::fixture::egress(routes);
                let found = enrich(&egress, musicbrainz::BASE, &recording, 640).await;
                (found, recorded.asked())
            }
        };
        let text = |e: &Enrichment| match &e.description {
            Found::Value(text, credit) => Some((
                text.clone(),
                credit.provider,
                credit.lang.clone().unwrap(),
                credit.subject.clone(),
            )),
            _ => None,
        };
        let wd = |text: &str, subject: Option<&String>| {
            Some((
                text.into(),
                CreditProvider::Wikidata,
                "en".into(),
                subject.cloned(),
            ))
        };
        let wp = |text: &str, lang: &str, subject: Option<&String>| {
            Some((
                text.into(),
                CreditProvider::Wikipedia,
                lang.into(),
                subject.cloned(),
            ))
        };
        // (a) The recording item's English article: its works are not read.
        let (e, asked) = found(vec![
            (
                q1.clone(),
                200,
                titled_pages("Q1", "Rec", &["en"], Some("rec line")),
            ),
            (summary_of("Rec", "en"), 200, titled_article("Rec", "en")),
            (works.clone(), 200, one_work()),
        ])
        .await;
        assert_eq!(text(&e), wp("Rec in en.", "en", None));
        assert!(!asked.contains(&works), "{asked:?}");
        // (b) Without one, the work's English article, naming the work; never the work's image.
        let (e, asked) = found(vec![
            (
                q1.clone(),
                200,
                titled_pages("Q1", "Rec", &["de"], Some("rec line")),
            ),
            (works.clone(), 200, one_work()),
            (
                q2.clone(),
                200,
                titled_pages("Q2", "Work", &["en"], Some("work line")),
            ),
            (summary_of("Work", "en"), 200, titled_article("Work", "en")),
        ])
        .await;
        assert_eq!(text(&e), wp("Work in en.", "en", Some(&subject)));
        assert_eq!(asked.iter().filter(|u| **u == works).count(), 1);
        assert!(asked.contains(&wikidata::claims_lookup("Q1", "P18")));
        assert!(!asked.contains(&wikidata::claims_lookup("Q2", "P18")));
        assert!(!asked.contains(&summary_of("Rec", "de")), "{asked:?}");
        // (b) missing (a 404): (c) the recording item's English description.
        let (e, _) = found(vec![
            (
                q1.clone(),
                200,
                titled_pages("Q1", "Rec", &[], Some("rec line")),
            ),
            (works.clone(), 200, one_work()),
            (
                q2.clone(),
                200,
                titled_pages("Q2", "Work", &["en"], Some("work line")),
            ),
            (summary_of("Work", "en"), 404, b"{}".to_vec()),
        ])
        .await;
        assert_eq!(text(&e), wd("rec line", None));
        // (d) The work's English description, naming the work.
        let (e, _) = found(vec![
            (q1.clone(), 200, titled_pages("Q1", "Rec", &["nl"], None)),
            (works.clone(), 200, one_work()),
            (
                q2.clone(),
                200,
                titled_pages("Q2", "Work", &["de"], Some("work line")),
            ),
        ])
        .await;
        assert_eq!(text(&e), wd("work line", Some(&subject)));
        // (e) Another Wikipedia: the recording item's first, alone.
        let (e, asked) = found(vec![
            (q1.clone(), 200, titled_pages("Q1", "Rec", &["nl"], None)),
            (works.clone(), 200, one_work()),
            (q2.clone(), 200, titled_pages("Q2", "Work", &["de"], None)),
            (summary_of("Rec", "nl"), 200, titled_article("Rec", "nl")),
            (summary_of("Work", "de"), 200, titled_article("Work", "de")),
        ])
        .await;
        assert_eq!(text(&e), wp("Rec in nl.", "nl", None));
        assert!(!asked.contains(&summary_of("Work", "de")), "{asked:?}");
        // Else the work's, naming the work.
        let (e, _) = found(vec![
            (q1.clone(), 200, titled_pages("Q1", "Rec", &["zh"], None)),
            (works.clone(), 200, one_work()),
            (q2.clone(), 200, titled_pages("Q2", "Work", &["de"], None)),
            (summary_of("Work", "de"), 200, titled_article("Work", "de")),
        ])
        .await;
        assert_eq!(text(&e), wp("Work in de.", "de", Some(&subject)));
        // An unread step ends the description unread, with nothing lower asked: the recording
        // item's article (works not read), the works, the work's item, the work's article.
        let rec = || {
            (
                q1.clone(),
                200,
                titled_pages("Q1", "Rec", &["en"], Some("rec line")),
            )
        };
        let no_article = || {
            (
                q1.clone(),
                200,
                titled_pages("Q1", "Rec", &[], Some("rec line")),
            )
        };
        let work = || {
            (
                q2.clone(),
                200,
                titled_pages("Q2", "Work", &["en"], Some("work line")),
            )
        };
        for (routes, works_read) in [
            (
                vec![rec(), (summary_of("Rec", "en"), 503, b"{}".to_vec())],
                false,
            ),
            (
                vec![no_article(), (works.clone(), 503, b"{}".to_vec())],
                true,
            ),
            (
                vec![
                    no_article(),
                    (works.clone(), 200, one_work()),
                    (q2.clone(), 503, b"{}".to_vec()),
                ],
                true,
            ),
            (
                vec![
                    no_article(),
                    (works.clone(), 200, one_work()),
                    work(),
                    (summary_of("Work", "en"), 503, b"{}".to_vec()),
                ],
                true,
            ),
        ] {
            let (e, asked) = found(routes).await;
            assert_eq!(e.description, Found::Unread, "{asked:?}");
            assert_eq!(asked.contains(&works), works_read, "{asked:?}");
        }
        // No item of its own: the work alone, and no image.
        let bare = entity_of(MbKind::Recording, RECORDING, &[]);
        let search = wikidata::search_lookup("P4404", RECORDING).unwrap();
        let none =
            json(serde_json::json!({"query": {"searchinfo": {"totalhits": 0}, "search": []}}));
        let (egress, _) = crate::sources::fixture::egress(vec![
            (search.as_str(), 200, "application/json", none),
            (works.as_str(), 200, "application/json", one_work()),
            (
                read_backs[1].0.as_str(),
                200,
                "application/json",
                read_backs[1].2.clone(),
            ),
            (
                q2.as_str(),
                200,
                "application/json",
                titled_pages("Q2", "Work", &[], Some("work line")),
            ),
        ]);
        let e = enrich(&egress, musicbrainz::BASE, &bare, 640).await;
        assert_eq!(e.image, Found::Missing);
        assert_eq!(text(&e), wd("work line", Some(&subject)));
    }
    /// A search hit at another level than the entity's is not its item: an album's item carrying
    /// an edition's release ID (it has a release group, Discogs master or work ID too), or an
    /// item carrying several release groups. Nothing past the read-back is asked.
    #[tokio::test(start_paused = true)]
    async fn an_item_at_another_level_is_not_the_entitys() {
        let mbid = "8e8a594f-2175-38c7-a871-abb68ec363e7";
        let other = "b11bf816-1844-47e5-9075-9239b7609c6a";
        let claim = |v: &str| serde_json::json!({"rank": "normal", "mainsnak": {"snaktype": "value", "datatype": "external-id", "datavalue": {"value": v}}});
        let item = |claims: serde_json::Value| {
            json(
                serde_json::json!({"entities": {"Q2": {"type": "item", "id": "Q2", "claims": claims}}}),
            )
        };
        let hit = json(
            serde_json::json!({"query": {"searchinfo": {"totalhits": 1}, "search": [{"title": "Q2"}]}}),
        );
        let read = wikidata::entities_lookup(&["Q2".into()]);
        let pages = wikidata::pages_lookup("Q2");
        for (kind, claims, own) in [
            (
                MbKind::Release,
                serde_json::json!({"P5813": [claim(mbid)]}),
                true,
            ),
            (
                MbKind::Release,
                serde_json::json!({"P5813": [claim(mbid)], "P436": [claim(other)]}),
                false,
            ),
            (
                MbKind::Release,
                serde_json::json!({"P5813": [claim(mbid)], "P435": [claim(other)]}),
                false,
            ),
            (
                MbKind::Release,
                serde_json::json!({"P5813": [claim(mbid)], "P1954": [claim("21491")]}),
                false,
            ),
            (
                MbKind::Release,
                serde_json::json!({"P5813": [claim(mbid), claim(other)]}),
                false,
            ),
            (
                MbKind::ReleaseGroup,
                serde_json::json!({"P436": [claim(mbid)]}),
                true,
            ),
            (
                MbKind::ReleaseGroup,
                serde_json::json!({"P436": [claim(mbid), claim(other)]}),
                false,
            ),
        ] {
            let search = wikidata::search_lookup(mbid_property(kind), mbid).unwrap();
            let (egress, recorded) = crate::sources::fixture::egress(vec![
                (search.as_str(), 200, "application/json", hit.clone()),
                (read.as_str(), 200, "application/json", item(claims.clone())),
            ]);
            let found = enrich(&egress, musicbrainz::BASE, &entity_of(kind, mbid, &[]), 640).await;
            assert_eq!(found, Enrichment::missing(), "{kind:?} {claims}");
            assert_eq!(recorded.asked().contains(&pages), own, "{kind:?} {claims}");
        }
    }
    /// An artist's Discogs pages name its item only when every page that names one names the
    /// same one, alone, and that item carries no other artist's MBID (none at all will do). A
    /// busy search on any of them leaves both fields unread. A read-back over the size cap
    /// verifies nothing: no item, not unread, and no Discogs lookup after the MBID's.
    #[tokio::test(start_paused = true)]
    async fn discogs_pages_name_one_item_or_none() {
        let mbid = "b5416a54-4941-4fcf-b2a0-7f98da6f9344";
        let artist = entity_of(
            MbKind::Artist,
            mbid,
            &[
                ("discogs", "https://www.discogs.com/artist/1"),
                ("discogs", "https://www.discogs.com/artist/2"),
                ("discogs", "https://www.discogs.com/artist/3"),
            ],
        );
        let mbid_search = wikidata::search_lookup("P434", mbid).unwrap();
        let discogs = |id: &str| wikidata::search_lookup("P1953", id).unwrap();
        let none =
            json(serde_json::json!({"query": {"searchinfo": {"totalhits": 0}, "search": []}}));
        let hit = |qid: &str| {
            json(
                serde_json::json!({"query": {"searchinfo": {"totalhits": 1}, "search": [{"title": qid}]}}),
            )
        };
        let crowded =
            json(serde_json::json!({"query": {"searchinfo": {"totalhits": 6}, "search": []}}));
        let claim = |v: &str| serde_json::json!([{"rank": "normal", "mainsnak": {"snaktype": "value", "datatype": "external-id", "datavalue": {"value": v}}}]);
        // Q1 names Discogs 1 and 2 and no MBID; Q2 names Discogs 2.
        let q1 = json(
            serde_json::json!({"entities": {"Q1": {"type": "item", "id": "Q1", "claims": {
            "P1953": [claim("1")[0].clone(), claim("2")[0].clone()]}}}}),
        );
        let q2 = json(
            serde_json::json!({"entities": {"Q2": {"type": "item", "id": "Q2", "claims": {
            "P1953": claim("2")}}}}),
        );
        let read = |qid: &str| wikidata::entities_lookup(&[qid.into()]);
        let run = |routes: Vec<(String, u16, &'static str, Vec<u8>)>| {
            let artist = artist.clone();
            async move {
                let routes: Vec<(&str, u16, &'static str, Vec<u8>)> = routes
                    .iter()
                    .map(|(u, s, k, b)| (u.as_str(), *s, *k, b.clone()))
                    .collect();
                let (egress, recorded) = crate::sources::fixture::egress(routes);
                let found = enrich(&egress, musicbrainz::BASE, &artist, 640).await;
                (found, recorded.asked())
            }
        };
        let j = "application/json";
        // Both pages name Q1, which carries no MBID: Q1. The third page is never looked up.
        let (_, asked) = run(vec![
            (mbid_search.clone(), 200, j, none.clone()),
            (discogs("1"), 200, j, hit("Q1")),
            (discogs("2"), 200, j, hit("Q1")),
            (read("Q1"), 200, j, q1.clone()),
        ])
        .await;
        assert!(asked.contains(&wikidata::pages_lookup("Q1")), "{asked:?}");
        assert!(!asked.contains(&discogs("3")), "{asked:?}");
        // Two items: none.
        let (found, asked) = run(vec![
            (mbid_search.clone(), 200, j, none.clone()),
            (discogs("1"), 200, j, hit("Q1")),
            (discogs("2"), 200, j, hit("Q2")),
            (read("Q1"), 200, j, q1.clone()),
            (read("Q2"), 200, j, q2.clone()),
        ])
        .await;
        assert_eq!(found, Enrichment::missing());
        assert!(
            asked.iter().all(|u| !u.contains("props=sitelinks")),
            "{asked:?}"
        );
        // One page names several items: none.
        let (found, asked) = run(vec![
            (mbid_search.clone(), 200, j, none.clone()),
            (discogs("1"), 200, j, crowded.clone()),
            (discogs("2"), 200, j, hit("Q1")),
            (read("Q1"), 200, j, q1.clone()),
        ])
        .await;
        assert_eq!(found, Enrichment::missing());
        assert!(
            asked.iter().all(|u| !u.contains("props=sitelinks")),
            "{asked:?}"
        );
        // A busy second search: unread.
        let (found, _) = run(vec![
            (mbid_search.clone(), 200, j, none.clone()),
            (discogs("1"), 200, j, hit("Q1")),
            (discogs("2"), 503, j, b"{}".to_vec()),
            (read("Q1"), 200, j, q1.clone()),
        ])
        .await;
        assert_eq!(found, Enrichment::unread());
        // The MBID's one hit is over the cap alone: no item, and no Discogs lookup.
        let (found, asked) = run(vec![
            (mbid_search.clone(), 200, j, hit("Q1")),
            (read("Q1"), 200, "!too-large", vec![]),
        ])
        .await;
        assert_eq!(found, Enrichment::missing());
        assert_eq!(asked, [mbid_search.clone(), read("Q1")]);
        // A Discogs page's one hit over the cap: no item.
        let (found, asked) = run(vec![
            (mbid_search.clone(), 200, j, none.clone()),
            (discogs("1"), 200, j, hit("Q1")),
            (read("Q1"), 200, "!too-large", vec![]),
        ])
        .await;
        assert_eq!(found, Enrichment::missing());
        assert!(
            asked.iter().all(|u| !u.contains("props=sitelinks")),
            "{asked:?}"
        );
    }
}
