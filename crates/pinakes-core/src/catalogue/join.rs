//! Joining links with catalogue cards. A link a catalogue holds, by its own link or by an exact
//! key read from the link alone, takes its card from the catalogue at once and is never
//! fetched: it is catalogue-backed (`resource.catalogue_card`) and follows its card. When the
//! card goes, or its catalogue is switched off, the link goes back to the fetch queue; the
//! references people made stay on it.
//!
//! Joins run when a link is referenced (`referenced`, after an ingest batch, and
//! `POST /v1/resolve`), before every fetch (`before_fetch`), and for the rows a card's write
//! names (`catalogue_join_due`, filled by the card table's triggers; `drain` works it off). Links
//! built from a catalogue switched off are given back by `sweep`, run in the background at start,
//! and so are those a check finds built from a catalogue that is off or dropped: their next reads
//! spread over a day. With every catalogue off, a join reads nothing; the daemon's joins task runs
//! the sweep and works the queue off once at start, then reads nothing more, and the extras task
//! reads nothing at all.
//!
//! What joins: the card whose `uri` is the link, then the card that holds the link's `url:` key,
//! then a card holding a key the link names ([`link_keys`]): the key's own catalogue first. Two
//! cards of one catalogue that tie are no join. A name, a title or any other metadata never
//! joins. A card typed as an event (an event's own schedule and route decide its card) backs no
//! link, and a link a source of people's own posts reads ([`social_link`]) is never built from a
//! card. OpenStreetMap's cards are left out: no resource row may hold an OSM-derived value. A
//! Podcast Index card makes its feed a related link only: the feed is fetched for its episodes.
//! Every link also takes the group keys of the cards it is a related link of.
use super::{link_key, stored_key, Catalogue, CataloguesConfig};
use crate::{
    config::QueueConfig,
    model::{self, CatalogueRef, ResourceMeta},
    queue::Job,
};
use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;

/// The catalogues a link is joined with: those that are on, without OpenStreetMap (its values
/// live only in `osm_place`).
pub fn joinable(config: &CataloguesConfig) -> Vec<&'static str> {
    config
        .enabled_names()
        .into_iter()
        .filter(|name| *name != Catalogue::Osm.name())
        .collect()
}

/// The source of a resource built from a catalogue card (`ResourceMeta::source`).
pub const SOURCE: &str = "catalogue";
/// The `resource_key.source` of the keys a catalogue card gives the resources built from it or
/// related to it.
pub const KEY_SOURCE: &str = "catalogue";
/// The most cards one link is related to that its keys name (`related_uris`).
pub const RELATED_CARDS: i64 = 4;
/// The most candidate cards one link's lookup reads.
const CANDIDATES: i64 = 32;

/// The exact keys a link names by itself, without a fetch, each with the catalogue whose own
/// key it is, in their stored form ([`stored_key`]): a MusicBrainz entity page's
/// `mb:<entity>:<mbid>`, a MusicBrainz ISRC page's `isrc:…`, a Wikidata item page's `wikidata:Q…`, an OpenStreetMap object's
/// `osm:<type>/<id>`, a DOI's `doi:…`, an arXiv page's `arxiv:…`, an OpenAlex work's
/// `openalex:W…`, a crate's `crate:…` (crates.io, docs.rs, lib.rs), an F-Droid app's
/// `android:…`, a Flathub app's `flathub:…`, and a site's homepage (`/`, no query) as
/// `homepage:<host without www.>`. A podcast feed and a Wikipedia article join by their link.
pub fn link_keys(uri: &str) -> Vec<(String, Catalogue)> {
    use crate::lookups::{apps, packages, papers};
    let mut keys: Vec<(String, Catalogue)> = vec![];
    if let Some((kind, mbid, _)) = crate::hubs::musicbrainz::entity_link(uri) {
        if let Some(key) = crate::hubs::key::mb(kind, &mbid) {
            keys.push((key, Catalogue::Musicbrainz));
        }
    }
    if let Some((isrc, _)) = crate::hubs::musicbrainz::isrc_link(uri) {
        keys.push((format!("isrc:{isrc}"), Catalogue::Musicbrainz));
    }
    if let Some((qid, _)) = crate::hubs::wikidata::item_link(uri) {
        if let Some(key) = crate::hubs::key::wikidata(&qid) {
            keys.push((key, Catalogue::Wikidata));
        }
    }
    if let Some(osm) = crate::resolver::osm::parse_id(uri) {
        keys.push((
            crate::lookups::places::key(&osm).to_string(),
            Catalogue::Osm,
        ));
    }
    if let Some(doi) = crate::resolver::cards::doi(uri) {
        keys.push((format!("doi:{doi}"), Catalogue::Openalex));
    }
    if let Some(id) = papers::arxiv_from_uri(uri) {
        keys.push((papers::arxiv_key(&id).to_string(), Catalogue::Openalex));
    }
    if let Some(work) = openalex_work(uri) {
        keys.push((format!("openalex:{work}"), Catalogue::Openalex));
    }
    if let Some(package) =
        packages::Package::from_uri(uri).filter(|p| p.registry == packages::Registry::Crates)
    {
        keys.push((package.key().to_string(), Catalogue::Software));
    }
    if let Some(app) = apps::app_from_uri(uri)
        .filter(|a| matches!(a.store, apps::Store::FDroid | apps::Store::Flathub))
    {
        keys.push((app.key().to_string(), Catalogue::Software));
    }
    if let Some(host) = homepage(uri) {
        keys.push((format!("homepage:{host}"), Catalogue::Tranco));
    }
    let mut seen = std::collections::BTreeSet::new();
    keys.into_iter()
        .filter_map(|(key, owner)| stored_key(&key).map(|key| (key, owner)))
        .filter(|(key, _)| seen.insert(key.clone()))
        .collect()
}
/// The OpenAlex work a link names: `openalex.org/W…`, `openalex.org/works/W…` and the API's and
/// explorer's `/works/W…` (upper case `W`, 1–12 digits).
fn openalex_work(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let id = match (host, segments.as_slice()) {
        ("openalex.org" | "www.openalex.org", [id]) => *id,
        (
            "openalex.org" | "www.openalex.org" | "api.openalex.org" | "explore.openalex.org",
            ["works", id],
        ) => *id,
        _ => return None,
    };
    let digits = id.strip_prefix(['W', 'w'])?;
    ((1..=12).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("W{digits}"))
}
/// The site a homepage link is: its host, without `www.` and a final dot, for an `http(s)` link
/// whose path is `/` with no query, user or port, on a host name with a dot (not an address).
pub(crate) fn homepage(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.path() != "/"
        || url.query().is_some()
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let url::Host::Domain(host) = url.host()? else {
        return None;
    };
    let host = host.trim_end_matches('.');
    let host = host.strip_prefix("www.").unwrap_or(host);
    (host.contains('.') && !host.starts_with('.') && host.len() <= 253).then(|| host.to_owned())
}

/// A catalogue card as a join reads it.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct CardRow {
    pub id: i64,
    pub catalogue: String,
    pub source: String,
    pub source_id: String,
    pub uri: String,
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub name: String,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub keys: Vec<String>,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub content_hash: Vec<u8>,
    /// A Wikipedia article the card names in its details (`details.wikipedia`).
    pub article: Option<String>,
}
const CARD_COLUMNS: &str = "id,catalogue,source,source_id,uri,type,name,description,labels,keys,ST_X(point) AS lon,ST_Y(point) AS lat,content_hash,details->>'wikipedia' AS article";

/// What a link's lookup found: the card it is built from, if any, and the group keys of the
/// cards it is a related link of (an official website, a podcast's feed).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub card: Option<Box<CardRow>>,
    pub related: Vec<String>,
}
/// Catalogues whose cards a link is never built from: their link is a feed a fetch reads for its
/// episodes. Such a card makes the link a related link (in its group, still fetched).
const FETCHED: &[&str] = &["podcastindex"];

/// How a card holds a link, best first: as its own link, by the link's `url:` key, by a key the
/// link names that is the card's own catalogue's, by one that is another's.
fn rank(card: &CardRow, uri: &str, own: Option<&str>, derived: &[(String, Catalogue)]) -> u8 {
    if card.uri == uri {
        return 0;
    }
    if own.is_some_and(|own| card.keys.iter().any(|k| k == own)) {
        return 1;
    }
    let owned = derived
        .iter()
        .any(|(key, owner)| owner.name() == card.catalogue && card.keys.contains(key));
    if owned {
        2
    } else {
        3
    }
}
/// The catalogues in the order a tie between them is settled: the catalogues the link's keys
/// belong to, then Wikidata, then the others in [`Catalogue::ALL`]'s order.
fn preference(derived: &[(String, Catalogue)]) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = derived.iter().map(|(_, owner)| owner.name()).collect();
    order.push(Catalogue::Wikidata.name());
    order.extend(Catalogue::ALL.iter().map(|c| c.name()));
    let mut seen = std::collections::BTreeSet::new();
    order.retain(|name| seen.insert(*name));
    order
}
/// The card a link takes among `cards` (those that hold its link or one of its keys): the best
/// ranked, by catalogue preference; none when two cards of the chosen catalogue tie.
pub fn pick(cards: Vec<CardRow>, uri: &str) -> Option<CardRow> {
    let own = link_key(uri);
    let derived = link_keys(uri);
    let ranked: Vec<(u8, CardRow)> = cards
        .into_iter()
        .map(|card| (rank(&card, uri, own.as_deref(), &derived), card))
        .collect();
    let best = ranked.iter().map(|(rank, _)| *rank).min()?;
    let catalogue = preference(&derived).into_iter().find(|name| {
        ranked
            .iter()
            .any(|(rank, card)| *rank == best && card.catalogue == *name)
    })?;
    let mut tied = ranked
        .into_iter()
        .filter(|(rank, card)| *rank == best && card.catalogue == catalogue)
        .map(|(_, card)| card);
    let card = tied.next()?;
    tied.next().is_none().then_some(card)
}
/// The card `uri` is built from among `catalogues`, and the cards it is a related link of.
pub async fn find(pool: &PgPool, catalogues: &[&str], uri: &str) -> Result<Found> {
    if catalogues.is_empty() {
        return Ok(Found::default());
    }
    let mut keys: Vec<String> = link_keys(uri).into_iter().map(|(key, _)| key).collect();
    keys.extend(link_key(uri));
    // The card whose link it is first, so no cap leaves it out. (`id+0`: the planner reads the
    // two indexes instead of walking the table in ID order.)
    let cards: Vec<CardRow> = sqlx::query_as(&format!(
        "SELECT {CARD_COLUMNS} FROM catalogue_card WHERE catalogue=ANY($1) AND (uri=$2 OR keys && $3::text[]) ORDER BY (uri=$2) DESC,id+0 LIMIT $4"
    ))
    .bind(catalogues)
    .bind(uri)
    .bind(&keys)
    .bind(CANDIDATES)
    .fetch_all(pool)
    .await?;
    let full = cards.len() as i64 >= CANDIDATES;
    let (fetched, cards): (Vec<CardRow>, Vec<CardRow>) = cards
        .into_iter()
        .partition(|c| FETCHED.contains(&c.catalogue.as_str()));
    // A card typed as an event backs no link: an event keeps its own schedule and card.
    let cards: Vec<CardRow> = cards.into_iter().filter(|c| !event_card(c)).collect();
    // So many cards hold the link that one of them may be missing: no join, unless one is the
    // card whose link it is.
    let card = pick(cards, uri).filter(|c| !full || c.uri == uri);
    // The cards that relate the link, at most 64 (`id+0`: read through the GIN index, never by
    // walking a catalogue in order); `related_keys` keeps one per catalogue.
    let mut related: Vec<(String, String)> = sqlx::query_as(
        "SELECT catalogue,source_id FROM catalogue_card WHERE catalogue=ANY($1) AND related_uris @> ARRAY[$2]::text[] ORDER BY id+0 LIMIT 64",
    )
    .bind(catalogues)
    .bind(uri)
    .fetch_all(pool)
    .await?;
    related.extend(
        fetched
            .iter()
            .map(|c| (c.catalogue.clone(), c.source_id.clone())),
    );
    Ok(Found {
        card: card.map(Box::new),
        related: related_keys(&related),
    })
}
/// Whether `card` is typed as an event (`Event` or one of its subtypes, `Festival`,
/// `SportsEvent`, …): such a card backs no link.
pub fn event_card(card: &CardRow) -> bool {
    model::type_path(&card.r#type)
        .iter()
        .any(|t| t == "Event")
}
/// The types a source's card of people's own posts has (`Source::social`): a post, a comment, a
/// profile or page (its bio is its owner's own text), a video.
const SOCIAL_TYPES: &[&str] = &[
    "SocialMediaPosting",
    "DiscussionForumPosting",
    "Comment",
    "Person",
    "Organization",
    "VideoObject",
    "Article",
    "CreativeWork",
];
/// Whether `uri` is a link a source of people's own posts reads (a Nostr object, a link of an
/// official source with social cards: Bluesky, Mastodon, Reddit, X, …): such a link keeps its own
/// card, with its scrubs and refresh, and is never built from a catalogue card, even before its
/// first read (when no stored card says it is social yet).
pub fn social_link(uri: &str) -> bool {
    if crate::resolver::classify(uri) == Some("nostr") {
        return true;
    }
    crate::sources::reading(uri).is_some_and(|source| {
        SOCIAL_TYPES.iter().any(|t| {
            let mut doc = ResourceMeta::new(uri, source.name(), 0);
            doc.set_type(t);
            source.social(&doc)
        })
    })
}
/// The group keys of the cards a link is a related link of: one per catalogue (a link two cards
/// of one catalogue relate takes neither's), at most [`RELATED_CARDS`].
fn related_keys(cards: &[(String, String)]) -> Vec<String> {
    let mut per: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (catalogue, source_id) in cards {
        let ids = per.entry(catalogue.as_str()).or_default();
        if !ids.contains(&source_id.as_str()) {
            ids.push(source_id);
        }
    }
    per.into_iter()
        .filter(|(_, ids)| ids.len() == 1)
        .filter_map(|(catalogue, ids)| card_key(catalogue, ids[0]))
        .take(RELATED_CARDS as usize)
        .collect()
}
/// The group key of one catalogue card, `catalogue:<catalogue>:<source id>` (a `:` or `%` in the
/// ID percent-encoded): every resource built from the card, and every resource whose link is one
/// of its related links, carries it, so they share a group. `catalogue:<catalogue>` names one
/// object (`groups::SINGLE_OBJECT`), so one link never brings two cards of a catalogue together.
pub fn card_key(catalogue: &str, source_id: &str) -> Option<String> {
    let id = source_id.replace('%', "%25").replace(':', "%3A");
    let key = format!("catalogue:{catalogue}:{id}");
    crate::also_on::valid_key(&key).then_some(key)
}
/// The group keys of the resources built from `card`: its keys that name the thing (not the
/// `url:` keys of the links people share for it) and its own [`card_key`].
pub fn backed_keys(card: &CardRow) -> Vec<String> {
    let mut keys: Vec<String> = card
        .keys
        .iter()
        .filter(|k| !k.starts_with("url:") && crate::also_on::valid_key(k))
        .cloned()
        .collect();
    keys.extend(card_key(&card.catalogue, &card.source_id));
    keys.sort();
    keys.dedup();
    keys
}

/// The card of `uri` built from `card`: its type, name, description, point and labels, and the
/// card it came from. It is final: a catalogue-backed card is never fetched. A description that
/// carries a credential is left out, and one too long for the document is cut; `None` when the
/// card still doesn't fit a document.
pub fn document(card: &CardRow, uri: &str) -> Option<ResourceMeta> {
    let mut doc = ResourceMeta::new(uri, SOURCE, 0);
    doc.set_type(&card.r#type);
    doc.name = Some(card.name.clone());
    doc.description = card
        .description
        .clone()
        .filter(|d| !crate::capability::contains_credential(d));
    doc.location = match (card.lat, card.lon) {
        (Some(lat), Some(lon)) => Some(model::Point { lat, lon }),
        _ => None,
    };
    doc.labels = card.labels.clone();
    doc.catalogue = Some(CatalogueRef {
        catalogue: card.catalogue.clone(),
        source_id: card.source_id.clone(),
        hash: hash_hex(&card.content_hash),
        summary: None,
    });
    fit(doc)
}
/// `doc` within the document's size: its description cut by halves, then without it, then
/// without labels; `None` when even that does not fit.
pub(crate) fn fit(mut doc: ResourceMeta) -> Option<ResourceMeta> {
    loop {
        if doc.validate(&[]).is_ok() {
            return Some(doc);
        }
        match doc.description.as_mut() {
            Some(text) if text.len() > 64 => {
                let mut cut = text.len() / 2;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push('…');
            }
            Some(_) => {
                doc.description = None;
                doc.credits
                    .retain(|c| c.field != crate::model::CreditField::Description);
            }
            None if !doc.labels.is_empty() => doc.labels.clear(),
            None => return None,
        }
    }
}
fn hash_hex(hash: &[u8]) -> String {
    hash.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// What a join did with one resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its card was built from a catalogue card (or its card is current).
    Built,
    /// It is a related link of one or more cards: it keeps its fetched card and joins their group.
    Related,
    /// It was built from a card and goes back to the fetch queue.
    Released,
    /// No card holds it, or it is not a link a card may hold.
    Nothing,
    /// Another write to the row came first (a claim, a save): nothing was changed.
    Raced,
    /// The card it would be built from makes no valid document: it is fetched instead.
    Invalid,
}

/// A resource as a join reads it.
#[derive(sqlx::FromRow)]
struct Linked {
    uri: String,
    resolver: Option<String>,
    catalogue_card: Option<i64>,
    alias: bool,
    osm: bool,
    /// An event of any kind, dated or not: events keep their own schedule and card.
    event: bool,
    built_hash: Option<String>,
    /// The catalogue of the card it is built from, as its stored card names it, and whether that
    /// catalogue has its state row (`pinakesd catalogue drop` deletes it).
    built_catalogue: Option<String>,
    built_state: bool,
    /// Its catalogue keys.
    keys: Vec<String>,
}

/// Join one resource with the catalogues `cfg.catalogues` (its root: an alias is never joined).
/// `claimed`: the generation a worker claimed the row at, whose fetch the join replaces; without
/// it the join fences any fetch under way (the generation bump; the lease stays the worker's).
pub async fn join(
    pool: &PgPool,
    cfg: &QueueConfig,
    id: &str,
    claimed: Option<i64>,
) -> Result<Outcome> {
    let Some(row) = sqlx::query_as::<_, Linked>(
        "SELECT uri,resolver,catalogue_card,EXISTS(SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) AS alias,osm_id IS NOT NULL AS osm,
          coalesce(type_path @> ARRAY['Event'],false) AS event,doc#>>'{catalogue,hash}' AS built_hash,doc#>>'{catalogue,catalogue}' AS built_catalogue,EXISTS(SELECT 1 FROM catalogue_state s WHERE s.catalogue=r.doc#>>'{catalogue,catalogue}') AS built_state,
          coalesce((SELECT array_agg(k.key ORDER BY k.key COLLATE \"C\") FROM resource_key k WHERE k.resource_id=r.id AND k.source='catalogue'),'{}') AS keys
         FROM resource r WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(Outcome::Nothing);
    };
    // A Pubky object, a point, an OSM object, an alias, a link no route reads, an event or a
    // calendar (its type, or the event routes) and a link that carries a credential hold no card
    // and join no group.
    let joins = !row.alias
        && !row.osm
        && !crate::capability::is_capability(&row.uri)
        && !(row.event && row.catalogue_card.is_none())
        && row
            .resolver
            .as_deref()
            .is_some_and(|r| !matches!(r, "pubky" | "geo" | "osm" | "event" | "event_platform"));
    let mut found = if joins {
        find(pool, &cfg.catalogues, &row.uri).await?
    } else {
        Found::default()
    };
    if found.card.is_some() || !found.related.is_empty() {
        // Social content (a post, even a gone one) keeps its own card, its scrubs and no group:
        // by its link before its first read, by its card after.
        let social = social_link(&row.uri) || {
            let doc: Option<Value> = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
                .bind(id)
                .fetch_optional(pool)
                .await?;
            doc.and_then(|d| serde_json::from_value::<ResourceMeta>(d).ok())
                .is_some_and(|d| {
                    crate::resolver::cards::social(&d) || crate::changes::gone_social(&d)
                })
        };
        if social {
            found = Found::default();
        }
    }
    // A page on an OpenStreetMap host holds no catalogue values (they are OSM's only in
    // `osm_place`): it may join a group, never be built from a card.
    let osm_host = url::Url::parse(&row.uri)
        .ok()
        .and_then(|u| u.host_str().map(crate::resolver::osm::osm_host))
        .unwrap_or(false);
    if osm_host {
        found.card = None;
    }
    let Some(card) = found.card else {
        // Built from a catalogue that is off now, or dropped: its next read is spread over a day,
        // as the start's sweep spreads them (no burst of fetches); a card removed upstream gives
        // its link back at once.
        let spread = !row.built_state
            || row
                .built_catalogue
                .as_deref()
                .is_some_and(|c| !cfg.catalogues.contains(&c));
        let released = row.catalogue_card.is_some() && release(pool, id, spread).await?;
        set_keys(pool, id, &row.keys, &found.related).await?;
        return Ok(if released {
            Outcome::Released
        } else if found.related.is_empty() {
            Outcome::Nothing
        } else {
            Outcome::Related
        });
    };
    // The keys of the cards it is a related link of, but not of another card of its own
    // card's catalogue (one group holds one card of a catalogue).
    let own = format!("catalogue:{}:", card.catalogue);
    let mut keys = backed_keys(&card);
    keys.extend(
        found
            .related
            .iter()
            .filter(|k| !k.starts_with(&own))
            .cloned(),
    );
    if row.catalogue_card == Some(card.id)
        && row.built_hash.as_deref() == Some(hash_hex(&card.content_hash).as_str())
    {
        set_keys(pool, id, &row.keys, &keys).await?;
        return Ok(Outcome::Built);
    }
    build(pool, cfg, id, &row.uri, &card, &keys, claimed).await
}
/// Build `id`'s card from `card` and mark it catalogue-backed, with the group keys `keys`.
async fn build(
    pool: &PgPool,
    cfg: &QueueConfig,
    id: &str,
    uri: &str,
    card: &CardRow,
    keys: &[String],
    claimed: Option<i64>,
) -> Result<Outcome> {
    let Some(mut doc) = document(card, uri) else {
        // A card that makes no valid card: the link is fetched (a row built from an earlier
        // version of it goes back to the queue).
        crate::telemetry::catalogue_join(&card.catalogue, "invalid");
        release(pool, id, false).await?;
        set_keys(pool, id, &[], &[]).await.ok();
        return Ok(Outcome::Invalid);
    };
    // The summary of the card's article read before stays while it is the same card's and the
    // same article's; another is read when it is not.
    let article = super::extras::article(card);
    let stored: Option<Value> = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let stored = stored.and_then(|v| serde_json::from_value::<ResourceMeta>(v).ok());
    let kept = stored.as_ref().is_some_and(|stored| {
        stored.catalogue.as_ref().is_some_and(|c| {
            c.catalogue == card.catalogue
                && c.source_id == card.source_id
                && c.summary.is_some()
                && c.summary == article
        })
    });
    if let Some(stored) = stored.as_ref().filter(|_| kept) {
        let mut with = doc.clone();
        super::extras::keep_summary(&mut with, stored);
        if let Some(with) = fit(with) {
            doc = with;
        }
    }
    // Marked in the join queue for a minute while the build runs: one cut off between its writes
    // (a cancelled request, a lost connection) is checked again. A row already marked keeps its
    // own mark.
    let guard: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "INSERT INTO catalogue_join_due(resource_id,due_at) VALUES($1,now()+interval '1 minute') ON CONFLICT (resource_id) DO NOTHING RETURNING marked_at",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let generation = match claimed {
        Some(generation) => generation,
        None => {
            let Some(generation) = sqlx::query_scalar::<_, i64>(
                "UPDATE resource SET generation=generation+1 WHERE id=$1 RETURNING generation",
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
            else {
                return Ok(Outcome::Raced);
            };
            generation
        }
    };
    // The save marks the row built from the card in the same write (`ResourceMeta::backing`).
    doc.backing = Some(card.id);
    crate::changes::save(pool, id, generation, &doc, cfg).await?;
    let built: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM resource WHERE id=$1 AND generation=$2 AND catalogue_card=$3)",
    )
    .bind(id)
    .bind(generation)
    .bind(card.id)
    .fetch_one(pool)
    .await?;
    if !built {
        return Ok(Outcome::Raced);
    }
    let mut tx = pool.begin().await?;
    write_keys(&mut tx, id, keys).await?;
    if let Some(article) = article.as_deref().filter(|_| !kept) {
        super::extras::mark(&mut tx, id, article).await?;
    }
    // The card the build read may have changed or gone meanwhile (its trigger's mark came before
    // this build's save): then the row is checked again now, else the build's own mark goes.
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM catalogue_card WHERE id=$1 AND content_hash=$2)",
    )
    .bind(card.id)
    .bind(&card.content_hash)
    .fetch_one(&mut *tx)
    .await?;
    if !current {
        sqlx::query("INSERT INTO catalogue_join_due(resource_id) VALUES($1) ON CONFLICT (resource_id) DO UPDATE SET due_at=now(),marked_at=clock_timestamp()")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else if let Some(marked) = guard {
        sqlx::query("DELETE FROM catalogue_join_due WHERE resource_id=$1 AND marked_at=$2")
            .bind(id)
            .bind(marked)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    crate::telemetry::catalogue_join(&card.catalogue, "built");
    if cfg.groups.enabled {
        crate::groups::reconcile(pool, id, &cfg.groups).await;
    }
    Ok(Outcome::Built)
}
/// Give a catalogue-backed resource back to the fetch queue: due now, or with `spread` at a time
/// within the next day its ID picks (the start's sweep's spread), its catalogue's labels and mark
/// taken off its card, which stays until the fetch replaces it. Whether it was backed.
async fn release(pool: &PgPool, id: &str, spread: bool) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let catalogue: Option<Option<String>> = sqlx::query_scalar(&format!(
        "WITH old AS (SELECT id,doc#>>'{{catalogue,catalogue}}' AS catalogue FROM resource WHERE id=$1 AND catalogue_card IS NOT NULL FOR UPDATE)
         UPDATE resource r SET catalogue_card=NULL,final=false,dead_letter=false,attempts=0,timeout_streak=0,refresh_at=CASE WHEN $2 THEN {SPREAD} ELSE now() END,next_attempt_at=CASE WHEN $2 THEN {SPREAD} ELSE now() END,priority_at=NULL,generation=r.generation+1
         FROM old WHERE r.id=old.id RETURNING old.catalogue",
    ))
    .bind(id)
    .bind(spread)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM catalogue_extras WHERE resource_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let Some(catalogue) = catalogue else {
        return Ok(false);
    };
    unmark(pool, id).await?;
    crate::telemetry::catalogue_join(catalogue.as_deref().unwrap_or("none"), "released");
    Ok(true)
}
/// Take a released row's catalogue mark and labels off its stored card, with a change line, as a
/// rewrite does; the card stays until the fetch replaces it.
async fn unmark(pool: &PgPool, id: &str) -> Result<()> {
    crate::changes::rewrite(
        pool,
        id,
        || async { anyhow::Ok(()) },
        |doc, ()| {
            let had = doc.catalogue.is_some() || !doc.labels.is_empty();
            doc.catalogue = None;
            doc.labels.clear();
            had
        },
    )
    .await?;
    Ok(())
}
/// Replace `id`'s catalogue keys `stored` with `keys`; nothing is written when they are the same.
async fn set_keys(pool: &PgPool, id: &str, stored: &[String], keys: &[String]) -> Result<()> {
    let mut wanted = keys.to_vec();
    wanted.sort();
    wanted.dedup();
    let mut have = stored.to_vec();
    have.sort();
    if have == wanted {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    write_keys(&mut tx, id, &wanted).await?;
    tx.commit().await?;
    Ok(())
}
async fn write_keys(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
    keys: &[String],
) -> Result<()> {
    let dropped = sqlx::query(
        "DELETE FROM resource_key WHERE resource_id=$1 AND source=$2 AND NOT key=ANY($3)",
    )
    .bind(id)
    .bind(KEY_SOURCE)
    .bind(keys)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if dropped > 0 {
        also_on_again(tx, &[id]).await?;
    }
    if keys.is_empty() {
        return Ok(());
    }
    // A key another source vouches for too is the catalogue's from now on: it never expires while
    // the card holds it, and that source's next run neither drops nor takes it back
    // (`also_on::record` changes only its own rows).
    sqlx::query("INSERT INTO resource_key(resource_id,key,source,via,expires_at) SELECT $1,k,$2,$2,'infinity' FROM unnest($3::text[]) k ON CONFLICT(resource_id,key) DO UPDATE SET key=excluded.key,source=excluded.source,via=excluded.via,seen_at=now(),expires_at=excluded.expires_at WHERE resource_key.source<>excluded.source")
        .bind(id)
        .bind(KEY_SOURCE)
        .bind(keys)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Queue the also-on sources of `ids` again: a catalogue key they took over is gone, and a source
/// that vouched for it records it again.
async fn also_on_again(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, ids: &[&str]) -> Result<()> {
    sqlx::query("INSERT INTO also_on_dirty(resource_id) SELECT unnest($1::text[]) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=least(also_on_dirty.next_attempt_at,now())")
        .bind(ids)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Join the resources just referenced (an ingest batch's links, a resolved link): those stored,
/// an alias by its root. Errors are logged: the link is joined again before its fetch.
pub async fn referenced(pool: &PgPool, cfg: &QueueConfig, ids: &[String]) -> usize {
    if cfg.catalogues.is_empty() || ids.is_empty() {
        return 0;
    }
    let roots: Vec<String> = match sqlx::query_scalar(
        "SELECT DISTINCT coalesce((SELECT a.root_id FROM resource_alias a WHERE a.alias_id=x),x) FROM unnest($1::text[]) x WHERE EXISTS(SELECT 1 FROM resource r WHERE r.id=x)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await
    {
        Ok(roots) => roots,
        Err(error) => {
            tracing::warn!(kind = crate::resolver::error_class(&anyhow::Error::from(error)), "the referenced links' roots were not read; they are joined before their fetch");
            return 0;
        }
    };
    let mut built = 0;
    for id in &roots {
        match join(pool, cfg, id, None).await {
            Ok(Outcome::Built) => built += 1,
            Ok(_) => {}
            Err(error) => {
                crate::telemetry::catalogue_join_error("referenced");
                tracing::warn!(resource_id = %id, kind = crate::resolver::error_class(&error), "joining a referenced link with the catalogues failed; it is joined before its fetch")
            }
        }
    }
    built
}
/// The claimed jobs still to fetch: a job whose link a card holds gets its card from the card
/// instead, with the claim's generation, and is not fetched; so is one whose row another join
/// took first. A failed join leaves its job to the fetch.
pub async fn before_fetch(pool: &PgPool, cfg: &QueueConfig, jobs: &[Job]) -> Vec<Job> {
    if cfg.catalogues.is_empty() {
        return jobs.to_vec();
    }
    let mut fetch = Vec::with_capacity(jobs.len());
    for job in jobs {
        match join(pool, cfg, &job.id, Some(job.generation)).await {
            Ok(Outcome::Built | Outcome::Raced) => {}
            Ok(_) => fetch.push(job.clone()),
            Err(error) => {
                crate::telemetry::catalogue_join_error("before_fetch");
                tracing::warn!(resource_id = %job.id, kind = crate::resolver::error_class(&error), "joining a link with the catalogues failed; it is fetched");
                fetch.push(job.clone());
            }
        }
    }
    fetch
}

/// Check up to `limit` rows of `catalogue_join_due` that are due: join each again (build it
/// again, join another card, or give it back to the fetch queue). A failed check is tried again
/// later, backing off to an hour; one another write raced is checked again from five seconds
/// later, doubling to an hour.
/// The rows checked.
pub async fn drain(pool: &PgPool, cfg: &QueueConfig, limit: i64) -> Result<usize> {
    let due: Vec<(String, i32, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT resource_id,attempts,marked_at FROM catalogue_join_due WHERE due_at<=now() ORDER BY due_at,resource_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for (id, attempts, marked) in &due {
        let retry = match join(pool, cfg, id, None).await {
            // Another write came first: checked again soon, later each time it does again.
            Ok(Outcome::Raced) => Some((
                5u64.saturating_mul(1 << (*attempts).clamp(0, 9)).min(3600),
                true,
            )),
            Ok(_) => None,
            Err(error) => {
                crate::telemetry::catalogue_join_error("drain");
                tracing::warn!(resource_id = %id, kind = crate::resolver::error_class(&error), "a catalogue join check failed; it is tried again");
                Some((
                    10u64.saturating_mul(1 << (*attempts).clamp(0, 9)).min(3600),
                    true,
                ))
            }
        };
        match retry {
            // A row marked again while it was checked stays for the next check.
            None => {
                sqlx::query("DELETE FROM catalogue_join_due WHERE resource_id=$1 AND marked_at=$2")
                    .bind(id)
                    .bind(marked)
                    .execute(pool)
                    .await?;
            }
            Some((wait, failed)) => {
                sqlx::query("UPDATE catalogue_join_due SET attempts=attempts+$3::int,due_at=now()+$2*interval '1 second' WHERE resource_id=$1")
                    .bind(id)
                    .bind(wait as f64)
                    .bind(i32::from(failed))
                    .execute(pool)
                    .await?;
            }
        }
    }
    Ok(due.len())
}
/// The rows the start's sweep writes per transaction.
const SWEEP_ROWS: i64 = 1000;
/// When a link given back by a catalogue switched off or dropped is read again: a time within the
/// next day its resource ID picks, so the links of a large catalogue are not fetched at once.
const SPREAD: &str = "now()+(('x'||substr(r.id,1,8))::bit(32)::bigint%86400)*interval '1 second'";
/// After the daemon's start, in the background, for the catalogues that are off and have ever been
/// imported (a state row; OpenStreetMap is never joined): give every resource built from one of
/// their cards back to the fetch queue, due at times spread over the next day (no burst of
/// fetches), take the catalogue's mark and labels off its card (with a change line) and its keys
/// off every resource. Meanwhile no answer shows such a catalogue's labels or `from` (`api`).
/// Nothing to do (every catalogue that ever ran is on) costs one read. The resources given back.
pub async fn sweep(pool: &PgPool, catalogues: &[&str]) -> Result<u64> {
    // OpenStreetMap is never joined, so nothing is built from its cards.
    let off: Vec<String> = sqlx::query_scalar(
        "SELECT catalogue FROM catalogue_state WHERE NOT catalogue=ANY($1) AND catalogue<>$2 ORDER BY catalogue",
    )
    .bind(catalogues)
    .bind(Catalogue::Osm.name())
    .fetch_all(pool)
    .await?;
    if off.is_empty() {
        return Ok(0);
    }
    let mut released = 0;
    // Paged by the backed rows' index from where the last page ended: a page never reads again the
    // rows of the catalogues still on it passed.
    let mut after: (i64, String) = (i64::MIN, String::new());
    loop {
        let mut tx = pool.begin().await?;
        let ids: Vec<(String, Option<String>, i64)> = sqlx::query_as(&format!(
            "WITH old AS (SELECT r.id,r.catalogue_card AS card,r.doc#>>'{{catalogue,catalogue}}' AS catalogue FROM resource r JOIN catalogue_card c ON c.id=r.catalogue_card
               WHERE r.catalogue_card IS NOT NULL AND (r.catalogue_card,r.id)>($3,$4) AND c.catalogue=ANY($1) ORDER BY r.catalogue_card,r.id LIMIT $2 FOR UPDATE OF r)
             UPDATE resource r SET catalogue_card=NULL,final=false,dead_letter=false,attempts=0,timeout_streak=0,priority_at=NULL,generation=r.generation+1,
               refresh_at={SPREAD},next_attempt_at={SPREAD}
             FROM old WHERE r.id=old.id RETURNING r.id,old.catalogue,old.card",
        ))
        .bind(&off)
        .bind(SWEEP_ROWS)
        .bind(after.0)
        .bind(&after.1)
        .fetch_all(&mut *tx)
        .await?;
        if let Some(last) = ids.iter().map(|(id, _, card)| (*card, id.clone())).max() {
            after = last;
        }
        let gone: Vec<&String> = ids.iter().map(|(id, _, _)| id).collect();
        sqlx::query("DELETE FROM catalogue_extras WHERE resource_id=ANY($1)")
            .bind(&gone)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM resource_key WHERE resource_id=ANY($1) AND source='catalogue'")
            .bind(&gone)
            .execute(&mut *tx)
            .await?;
        let gone: Vec<&str> = gone.iter().map(|g| g.as_str()).collect();
        also_on_again(&mut tx, &gone).await?;
        tx.commit().await?;
        for (id, catalogue, _) in &ids {
            unmark(pool, id).await?;
            crate::telemetry::catalogue_join(catalogue.as_deref().unwrap_or("none"), "released");
        }
        released += ids.len() as u64;
        if (ids.len() as i64) < SWEEP_ROWS {
            break;
        }
    }
    // The related links' keys of those catalogues (`catalogue:<name>:…`): the catalogue keys in
    // the order of their index, a page at a time from where the last ended (never from the start
    // again), each page's keys of those catalogues deleted.
    let prefixes: Vec<String> = off.iter().map(|c| format!("catalogue:{c}:")).collect();
    let mut after = (String::new(), String::new());
    loop {
        let page: Vec<(String, String)> = sqlx::query_as(
            "SELECT key,resource_id FROM resource_key WHERE source='catalogue' AND key>=$1 AND (key,resource_id)>($1,$2) ORDER BY key,resource_id LIMIT $3",
        )
        .bind(&after.0)
        .bind(&after.1)
        .bind(SWEEP_ROWS)
        .fetch_all(pool)
        .await?;
        let Some(last) = page.last().cloned() else {
            break;
        };
        let (keys, ids): (Vec<&str>, Vec<&str>) = page
            .iter()
            .filter(|(key, _)| prefixes.iter().any(|p| key.starts_with(p.as_str())))
            .map(|(key, id)| (key.as_str(), id.as_str()))
            .unzip();
        if !keys.is_empty() {
            sqlx::query("DELETE FROM resource_key k USING unnest($1::text[],$2::text[]) AS x(key,resource_id) WHERE k.key=x.key AND k.resource_id=x.resource_id AND k.source='catalogue'")
                .bind(&keys)
                .bind(&ids)
                .execute(pool)
                .await?;
        }
        if (page.len() as i64) < SWEEP_ROWS {
            break;
        }
        after = last;
    }
    Ok(released)
}

/// The card of a link nobody shared, read from the catalogues by its resource ID: the card whose
/// own link has that ID (its `url:` key; [`link_key`]), as [`pick`] takes it. Nothing is stored.
/// A link that carries a credential is never answered.
pub async fn unshared(pool: &PgPool, catalogues: &[&str], id: &str) -> Result<Option<CardRow>> {
    if catalogues.is_empty() || !model::valid_id(id) {
        return Ok(None);
    }
    let cards: Vec<CardRow> = sqlx::query_as(&format!(
        "SELECT {CARD_COLUMNS} FROM catalogue_card WHERE catalogue=ANY($1) AND keys @> ARRAY['url:'||$2]::text[] ORDER BY id+0 LIMIT $3"
    ))
    .bind(catalogues)
    .bind(id)
    .bind(CANDIDATES)
    .fetch_all(pool)
    .await?;
    // Not a link that carries a credential, nor a page on an OpenStreetMap host (no card values
    // there: they are OSM's only in `osm_place`).
    let own: Vec<CardRow> = cards
        .into_iter()
        .filter(|c| model::identity(&c.uri).is_ok_and(|(own, _, _)| own == id))
        .filter(|c| !crate::capability::is_capability(&c.uri) && !social_link(&c.uri))
        .filter(|c| {
            !url::Url::parse(&c.uri)
                .ok()
                .and_then(|u| u.host_str().map(crate::resolver::osm::osm_host))
                .unwrap_or(false)
        })
        .collect();
    let Some(uri) = own.first().map(|c| c.uri.clone()) else {
        return Ok(None);
    };
    Ok(pick(own, &uri))
}
/// A stored resource in `card`'s group: one built from it, else one that is one of its related
/// links, one in a group first; the group an unshared card answers with.
pub async fn member(pool: &PgPool, card: &CardRow) -> Result<Option<String>> {
    if let Some(id) =
        sqlx::query_scalar("SELECT r.id FROM resource r WHERE r.catalogue_card=$1 ORDER BY EXISTS(SELECT 1 FROM group_unit u WHERE u.resource_id=r.id AND u.group_id IS NOT NULL) DESC,r.id LIMIT 1")
            .bind(card.id)
            .fetch_optional(pool)
            .await?
    {
        return Ok(Some(id));
    }
    let Some(key) = card_key(&card.catalogue, &card.source_id) else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar(
        "SELECT k.resource_id FROM resource_key k WHERE k.key=$1 AND k.source='catalogue' ORDER BY EXISTS(SELECT 1 FROM group_unit u WHERE u.resource_id=k.resource_id AND u.group_id IS NOT NULL) DESC,k.resource_id LIMIT 1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored card of Wikidata and a tagged link built from it: the card as the join read it.
    async fn built(pool: &PgPool) -> (String, CardRow) {
        let uri = "https://en.wikipedia.org/wiki/Douglas_Adams";
        let card = super::super::Card {
            source_id: "Q42".into(),
            uri: uri.into(),
            r#type: "Person".into(),
            name: "Douglas Adams".into(),
            labels: vec!["writer".into()],
            keys: vec!["wikidata:Q42".into()],
            ..Default::default()
        };
        super::super::store::upsert(pool, Catalogue::Wikidata, 1, &[card], None)
            .await
            .unwrap();
        let id = crate::store::ensure_resource(pool, uri).await.unwrap();
        sqlx::query("DELETE FROM catalogue_join_due")
            .execute(pool)
            .await
            .unwrap();
        let row: CardRow = sqlx::query_as(&format!("SELECT {CARD_COLUMNS} FROM catalogue_card"))
            .fetch_one(pool)
            .await
            .unwrap();
        (id, row)
    }
    fn cfg() -> QueueConfig {
        QueueConfig {
            catalogues: vec!["wikidata"],
            ..Default::default()
        }
    }
    async fn marked(pool: &PgPool, id: &str) -> Option<bool> {
        sqlx::query_scalar("SELECT due_at<=now() FROM catalogue_join_due WHERE resource_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// The card changed between the join's read and its save: the row is checked again now.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_build_from_a_card_that_changed_meanwhile_is_checked_again(pool: PgPool) {
        let (id, mut card) = built(&pool).await;
        card.content_hash = vec![7; 32];
        let keys = backed_keys(&card);
        assert_eq!(
            build(&pool, &cfg(), &id, &card.uri.clone(), &card, &keys, None)
                .await
                .unwrap(),
            Outcome::Built
        );
        assert_eq!(marked(&pool, &id).await, Some(true));
        drain(&pool, &cfg(), 10).await.unwrap();
        let hash: Option<String> =
            sqlx::query_scalar("SELECT doc#>>'{catalogue,hash}' FROM resource WHERE id=$1")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let stored: Vec<u8> = sqlx::query_scalar("SELECT content_hash FROM catalogue_card")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(hash, Some(hash_hex(&stored)));
        assert_eq!(marked(&pool, &id).await, None);
    }

    /// A build another write raced keeps its mark in the join queue: it is checked again later.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_build_cut_off_leaves_its_row_marked(pool: PgPool) {
        let (id, card) = built(&pool).await;
        let keys = backed_keys(&card);
        // A claim whose generation another write moved past: its save writes nothing.
        let outcome = build(
            &pool,
            &cfg(),
            &id,
            &card.uri.clone(),
            &card,
            &keys,
            Some(-1),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Raced);
        assert_eq!(marked(&pool, &id).await, Some(false));
    }

    /// A backed row whose card no longer makes a valid card goes back to the fetch queue.
    #[sqlx::test(migrations = "../../migrations")]
    async fn a_card_that_becomes_invalid_gives_its_row_back(pool: PgPool) {
        let (id, card) = built(&pool).await;
        assert_eq!(
            join(&pool, &cfg(), &id, None).await.unwrap(),
            Outcome::Built
        );
        let mut huge = card.clone();
        huge.name = "x".repeat(9000);
        huge.content_hash = vec![9; 32];
        let keys = backed_keys(&huge);
        assert_eq!(
            build(&pool, &cfg(), &id, &card.uri.clone(), &huge, &keys, None)
                .await
                .unwrap(),
            Outcome::Invalid
        );
        let (backing, due): (Option<i64>, bool) = sqlx::query_as(
            "SELECT catalogue_card,next_attempt_at<=now() FROM resource WHERE id=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((backing, due), (None, true));
    }

    fn card(id: i64, catalogue: &str, uri: &str, keys: &[&str]) -> CardRow {
        CardRow {
            id,
            catalogue: catalogue.into(),
            source: catalogue.into(),
            source_id: format!("s{id}"),
            uri: uri.into(),
            r#type: "Thing".into(),
            name: format!("card {id}"),
            description: None,
            labels: vec![],
            keys: keys.iter().map(|k| k.to_string()).collect(),
            lon: None,
            lat: None,
            content_hash: vec![0; 32],
            article: None,
        }
    }

    #[test]
    fn links_name_their_keys_without_a_fetch() {
        let keys =
            |uri: &str| -> Vec<String> { link_keys(uri).into_iter().map(|(k, _)| k).collect() };
        assert_eq!(
            keys("https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69"),
            ["mb:recording:b1a9c0e9-d987-4042-ae91-78d6a3267d69"]
        );
        assert_eq!(keys("https://www.wikidata.org/wiki/Q42"), ["wikidata:Q42"]);
        assert_eq!(
            keys("https://www.openstreetmap.org/node/240109189"),
            ["osm:node/240109189"]
        );
        assert_eq!(keys("doi:10.1000/ABC"), ["doi:10.1000/abc"]);
        assert_eq!(
            keys("https://arxiv.org/abs/2302.04668v2"),
            ["arxiv:2302.04668"]
        );
        assert_eq!(
            keys("https://openalex.org/W2741809807"),
            ["openalex:W2741809807"]
        );
        assert_eq!(
            keys("https://api.openalex.org/works/w2741809807"),
            ["openalex:W2741809807"]
        );
        assert_eq!(keys("https://crates.io/crates/serde"), ["crate:serde"]);
        assert_eq!(
            keys("https://f-droid.org/packages/org.fdroid.fdroid/"),
            ["android:org.fdroid.fdroid"]
        );
        assert_eq!(
            keys("https://flathub.org/apps/org.gnome.Maps"),
            ["flathub:org.gnome.Maps"]
        );
        assert_eq!(keys("https://www.example.org/"), ["homepage:example.org"]);
        assert_eq!(keys("http://example.org/"), ["homepage:example.org"]);
        // A page under a site, a query, a port, an address, a Play Store page: no key.
        for uri in [
            "https://example.org/about",
            "https://example.org/?q=1",
            "https://example.org:8443/",
            "https://192.0.2.1/",
            "https://localhost/",
            "https://play.google.com/store/apps/details?id=org.fdroid.fdroid",
            "https://en.wikipedia.org/wiki/Douglas_Adams",
            "https://openalex.org/A123",
            "https://openalex.org/W",
            "https://openalex.org/W1234567890123",
        ] {
            assert!(keys(uri).is_empty(), "{uri}: {:?}", keys(uri));
        }
        // Each key is in its stored form, and names its own catalogue.
        for (key, owner) in link_keys("https://www.wikidata.org/wiki/Q42")
            .into_iter()
            .chain(link_keys("https://crates.io/crates/serde"))
        {
            assert_eq!(stored_key(&key).as_deref(), Some(key.as_str()));
            assert!(matches!(owner, Catalogue::Wikidata | Catalogue::Software));
        }
    }

    /// Random links over the hosts and path forms the readers know, and junk: reading their keys
    /// never panics, every key is valid and in its stored form, names a catalogue a reader
    /// knows, and a homepage key comes only from a `/` link without a query.
    #[test]
    fn any_link_gives_valid_stored_keys() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let hosts = [
            "musicbrainz.org",
            "www.wikidata.org",
            "www.openstreetmap.org",
            "doi.org",
            "arxiv.org",
            "openalex.org",
            "api.openalex.org",
            "crates.io",
            "docs.rs",
            "f-droid.org",
            "flathub.org",
            "www.example.org",
            "example.org.",
            "xn--bcher-kva.example",
            "192.0.2.1",
            "[2001:db8::1]",
            "localhost",
            "en.wikipedia.org",
        ];
        let parts = [
            "",
            "/",
            "artist",
            "recording",
            "release-group",
            "e9ed318d-8cc5-4cf8-ab77-505e39ab6ea4",
            "wiki",
            "Q42",
            "Q0",
            "node",
            "way",
            "240109189",
            "10.1000",
            "ABC%2Fdef",
            "abs",
            "2302.04668v2",
            "W2741809807",
            "w1",
            "works",
            "crates",
            "serde",
            "packages",
            "org.fdroid.fdroid",
            "apps",
            "org.gnome.Maps",
            "?q=1",
            "#x",
            "%00",
            "..",
            "ü",
            " ",
            ":",
            "@",
            "W",
            "W1234567890123",
        ];
        let schemes = ["https://", "http://", "doi:", "ftp://", "", "HTTPS://"];
        for _ in 0..20_000 {
            let mut uri = schemes[next() as usize % schemes.len()].to_owned();
            uri.push_str(hosts[next() as usize % hosts.len()]);
            for _ in 0..(next() % 5) {
                if next() % 2 == 0 {
                    uri.push('/');
                }
                uri.push_str(parts[next() as usize % parts.len()]);
            }
            let Ok((_, normal, _)) = model::identity(&uri) else {
                continue;
            };
            for (key, owner) in link_keys(&normal) {
                assert!(super::super::valid_key(&key), "{normal}: {key}");
                assert_eq!(stored_key(&key).as_deref(), Some(key.as_str()), "{normal}");
                assert!(crate::also_on::valid_key(&key), "{normal}: {key}");
                if key.starts_with("homepage:") {
                    let url = url::Url::parse(&normal).unwrap();
                    assert!(url.path() == "/" && url.query().is_none(), "{normal}");
                    assert_eq!(owner, Catalogue::Tranco);
                }
            }
            let _ = link_key(&normal);
        }
    }

    #[test]
    fn the_best_held_card_wins_and_a_tie_in_one_catalogue_is_no_join() {
        let uri = "https://musicbrainz.org/artist/5b11f4ce-a62d-471e-81fc-a69a8278c7da";
        let own = link_key(uri).unwrap();
        let mb = "mb:artist:5b11f4ce-a62d-471e-81fc-a69a8278c7da";
        // The card whose link it is beats a card holding its key.
        let by_link = card(1, "musicbrainz", uri, &[]);
        let by_key = card(
            2,
            "wikidata",
            "https://en.wikipedia.org/wiki/Nirvana_(band)",
            &[mb],
        );
        assert_eq!(
            pick(vec![by_key.clone(), by_link.clone()], uri).unwrap().id,
            1
        );
        // The link's `url:` key beats a key it names.
        let by_url = card(3, "wikidata", "https://en.wikipedia.org/wiki/X", &[&own]);
        assert_eq!(pick(vec![by_key.clone(), by_url], uri).unwrap().id, 3);
        // A key the link names: its own catalogue's card first.
        let owned = card(4, "musicbrainz", "https://musicbrainz.org/artist/x", &[mb]);
        assert_eq!(
            pick(vec![by_key.clone(), owned.clone()], uri).unwrap().id,
            4
        );
        // Another catalogue's card when the own one is not there.
        assert_eq!(pick(vec![by_key.clone()], uri).unwrap().id, 2);
        // Two cards of one catalogue tie: no join.
        let twin = card(5, "musicbrainz", "https://musicbrainz.org/artist/y", &[mb]);
        assert_eq!(pick(vec![owned, twin], uri), None);
        assert_eq!(pick(vec![], uri), None);
    }

    #[test]
    fn card_keys_are_valid_group_keys() {
        assert_eq!(
            card_key("wikidata", "Q42").as_deref(),
            Some("catalogue:wikidata:Q42")
        );
        assert_eq!(
            card_key("software", "crates.io:serde").as_deref(),
            Some("catalogue:software:crates.io%3Aserde")
        );
        assert_eq!(
            card_key("musicbrainz", "recording/b1a9").as_deref(),
            Some("catalogue:musicbrainz:recording/b1a9")
        );
        assert_eq!(card_key("wikidata", "a b"), None);
        let c = card(
            1,
            "wikidata",
            "https://en.wikipedia.org/wiki/X",
            &["wikidata:Q42", "url:00000000000000000000000000000000"],
        );
        assert_eq!(backed_keys(&c), ["catalogue:wikidata:s1", "wikidata:Q42"]);
    }

    #[test]
    fn a_built_card_is_final_and_names_its_card() {
        let mut c = card(
            1,
            "wikidata",
            "https://en.wikipedia.org/wiki/Douglas_Adams",
            &[],
        );
        c.r#type = "Person".into();
        c.description = Some("English writer".into());
        c.labels = vec!["writer".into()];
        c.lat = Some(51.5);
        c.lon = Some(-0.1);
        let doc = document(&c, "https://en.wikipedia.org/wiki/Douglas_Adams").unwrap();
        assert_eq!(doc.type_path, ["Thing", "Person"]);
        assert_eq!(doc.name.as_deref(), Some("card 1"));
        assert_eq!(doc.ttl_secs, 0);
        assert_eq!(doc.source, SOURCE);
        assert_eq!(doc.labels, ["writer"]);
        assert_eq!(doc.catalogue.as_ref().unwrap().source_id, "s1");
        assert!(doc.location.is_some());
        doc.validate(&[]).unwrap();
        // An unknown type keeps the plain web page type.
        c.r#type = "NoSuchType".into();
        assert_eq!(document(&c, &c.uri.clone()).unwrap().type_name, "WebPage");
        // A description that carries a credential is left out; one too long is cut to fit.
        c.description = Some("see https://example.org/feed?token=abc".into());
        assert_eq!(document(&c, &c.uri.clone()).unwrap().description, None);
        c.description = Some("\"".repeat(5000));
        c.name = "\\".repeat(500);
        c.source_id = "x".repeat(500);
        let fitted = document(&c, &c.uri.clone()).unwrap();
        fitted.validate(&[]).unwrap();
        assert!(fitted.description.as_ref().is_none_or(|d| d.len() < 5000));
        let related = [
            ("wikidata".to_owned(), "Q1".to_owned()),
            ("wikidata".to_owned(), "Q2".to_owned()),
            ("musicbrainz".to_owned(), "a".to_owned()),
        ];
        assert_eq!(related_keys(&related), ["catalogue:musicbrainz:a"]);
    }

    /// Property, both ways over generated keys of every kind a link names (case, `_` and `-` in a
    /// crate's name, arXiv versions, IDN hosts): each link `catalogue_key_links` names for a key is
    /// normalized and names the key back (`link_keys`), so a row stored under it is marked when a
    /// card with the key is written; and the canonical spelling of a link (lower case, a crate's
    /// name with one kind of separator) is among the links of each key it names, as its root (a
    /// `doi.org` link's `doi:` root).
    #[sqlx::test(migrations = "../../migrations")]
    async fn catalogue_key_links_and_link_keys_agree_both_ways(pool: PgPool) {
        use super::super::testgen::{spelled, Gen};
        let mut gen = Gen::new("key links", 0x1c69_b3f7_4ac4_5a1d);
        let spelled: Vec<_> = (0..3000).map(|_| spelled(&mut gen, true)).collect();
        // OpenStreetMap is never joined: no links are named for its keys.
        let keys: Vec<String> = spelled
            .iter()
            .filter(|s| s.owner != Catalogue::Osm)
            .map(|s| s.key.clone())
            .collect();
        let named: Vec<(String, Vec<String>)> = sqlx::query_as(
            "SELECT k,coalesce(catalogue_key_links(k),'{}') FROM unnest($1::text[]) k",
        )
        .bind(&keys)
        .fetch_all(&pool)
        .await
        .unwrap();
        let links: std::collections::HashMap<String, Vec<String>> = named.into_iter().collect();
        for (key, named) in &links {
            assert!(!named.is_empty(), "{key} names no link");
            for link in named {
                let (_, normal, _) = model::identity(link).unwrap();
                assert_eq!(&normal, link, "{key}: {link} not normalized");
                assert!(
                    link_keys(link).iter().any(|(k, _)| k == key),
                    "{key}: {link} reads {:?}",
                    link_keys(link)
                );
            }
        }
        for s in spelled.iter().filter(|s| s.owner != Catalogue::Osm) {
            let (_, normal, _) = model::identity(&s.link).unwrap();
            let root = crate::alias::root(&normal).map_or(normal, |a| a.root);
            for (key, _) in link_keys(&s.link) {
                let named = &links[&key];
                assert!(named.contains(&root), "{}: {root} not among {key}'s {named:?}", s.link);
            }
        }
    }
}
