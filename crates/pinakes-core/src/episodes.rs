//! A podcast show's latest episodes, so a client can list and play them from the show's card.
//! They are read from the show's feed as the show's card is built (the
//! feed's own card in `resolver::cards::podcast_body`; a Podcast Index or Apple Podcasts show's
//! card fetches the feed its record names), kept in `podcast_episode` (the latest [`KEPT`],
//! replaced whole at each save of the show, so they refresh with it) and served by
//! `GET /v1/resources/{id}/episodes` (docs/api.md, Episodes); every show card carries the first
//! [`ON_CARD`] that have a stream (`Card::episodes`, filled by [`on_cards`] from the stored rows).
//!
//! Why a route and a table, not a fact: a fact is one string of at most 512 bytes per name and a
//! card's document is bounded at 8 KiB; twenty episodes with their streams are neither. Why not
//! occurrences: an episode has no instant to window by, no recurrence and no document of its
//! own here; the list is the feed's head and nothing more. An entry is not a card: the episode's
//! card is its `uri` resolved (`POST /v1/resolve`), and `id` names that card when Pinakes has it.
use crate::model::Stream;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use std::time::Duration;
use url::Url;
use utoipa::ToSchema;

/// How many of a show's latest episodes are kept and listed.
pub const KEPT: usize = 20;
/// How many of a feed's items are looked at for them: feeds list newest first, and a few are
/// out of order.
const SCANNED: usize = 200;

/// One of a show's latest episodes, as its feed lists it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct Episode {
    /// The episode's own card, when Pinakes has one for its `uri` (an alias reads as its root);
    /// absent until someone looks it up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The episode's page (the item's link), else its feed item (`{feed}?guid={guid}`), which
    /// resolves to its `PodcastEpisode` card.
    pub uri: String,
    /// Plain text, entity-decoded: escape it before rendering it as HTML.
    pub name: Option<String>,
    pub image: Option<String>,
    pub published: Option<DateTime<Utc>>,
    /// What names the item ([`item_identity`]): its GUID as the feed gives it; its page when it
    /// gives none; else `sha256:` and the hex digest of its enclosure's URL, so a signed link
    /// shows nothing of itself here.
    pub guid: String,
    /// Its enclosure, when public and direct (`Stream`); null when the item has none Pinakes
    /// would hand to a player.
    pub stream: Option<Stream>,
}

/// The latest [`KEPT`] episodes among a feed's items: newest first by `pubDate`, undated ones
/// after them in the feed's order; an item without a GUID, an enclosure or a link is no
/// episode.
pub fn from_items(items: &[roxmltree::Node<'_, '_>], feed: &Url) -> Vec<Episode> {
    let mut episodes: Vec<(usize, Episode)> = items
        .iter()
        .take(SCANNED)
        .enumerate()
        .filter_map(|(n, item)| Some((n, episode(*item, feed)?)))
        .collect();
    episodes.sort_by(|a, b| b.1.published.cmp(&a.1.published).then(a.0.cmp(&b.0)));
    episodes.truncate(KEPT);
    episodes.into_iter().map(|(_, e)| e).collect()
}
/// The episodes a fetched feed lists: an RSS channel's items, a body cut at the fetch cap read
/// to its last whole item. `None` when the body is no feed.
pub fn from_feed(f: &crate::resolver::egress::Fetched) -> Option<Vec<Episode>> {
    let text = String::from_utf8_lossy(&f.body);
    let completed;
    let body: &str = if f.truncated {
        completed = crate::resolver::cards::feed_prefix(&text, SCANNED)?;
        &completed
    } else {
        &text
    };
    let xml = roxmltree::Document::parse(body).ok()?;
    if !xml.root_element().has_tag_name("rss")
        || !xml.descendants().any(|n| n.has_tag_name("channel"))
    {
        return None;
    }
    let items: Vec<_> = xml
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "item")
        .take(SCANNED)
        .collect();
    Some(from_items(&items, &f.url))
}
/// The longest a show's read gives its feed's episodes: the fetch, the wait for a parse permit
/// and the parse together. The list is optional: a feed slower than this leaves the show's
/// stored list as it is, and the show's card is saved all the same. A read with a deadline of
/// its own (a wall hook under the web fetch's, `sources::Context::deadline`) gives the feed what
/// is left of it, less [`HANDBACK`], and never more than this.
pub const FEED_WAIT: Duration = Duration::from_secs(3);
/// What a hook keeps of its deadline to hand the card back.
pub const HANDBACK: Duration = Duration::from_millis(300);

#[cfg(test)]
tokio::task_local! {
    /// A test's wait before the feed's parse, as if every parser were busy: a wait a test can
    /// set for its own task alone. (Holding every parse permit instead would make the paused
    /// clocks of the tests running beside it advance across their own waits for a parser.)
    pub(crate) static PARSE_WAIT: Duration;
}

/// The deadline a show's read gives its feed: `deadline` less [`HANDBACK`] when the read has
/// one, and at most [`FEED_WAIT`] from now.
pub fn feed_deadline(deadline: Option<tokio::time::Instant>) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    let latest = now + FEED_WAIT;
    match deadline {
        Some(d) => d.checked_sub(HANDBACK).unwrap_or(now).min(latest),
        None => latest,
    }
}

/// The episodes of the feed at `url`, read through the egress (a discovery, cached for `ttl`
/// seconds, cut at the page cap like any feed) and parsed, all before `deadline`
/// ([`feed_deadline`]); `None` when the feed could not be read or parsed in time or is none: the
/// show's stored list then stays as it is.
pub async fn fetch(
    egress: &crate::resolver::egress::Egress,
    url: &str,
    ttl: u64,
    deadline: tokio::time::Instant,
) -> Option<Vec<Episode>> {
    let read = async {
        // As the media route reads a feed: a feed body over the page cap is cut there, not
        // refused (the egress reads an XML feed without a feed cap like a page).
        let options = Default::default();
        let f = match egress.discover(url, &options, ttl).await {
            Ok(f) if f.status == 200 => f,
            Ok(f) => {
                tracing::debug!(host = %crate::resolver::log_host(url), status = f.status, "a show's feed did not answer its episodes");
                return None;
            }
            Err(error) => {
                tracing::debug!(host = %crate::resolver::log_host(url), kind = crate::resolver::error_class(&error), "a show's feed could not be read for its episodes");
                return None;
            }
        };
        #[cfg(test)]
        if let Ok(wait) = PARSE_WAIT.try_with(|d| *d) {
            tokio::time::sleep(wait).await;
        }
        crate::resolver::html::blocking(move || from_feed(&f))
            .await
            .ok()
            .flatten()
    };
    match tokio::time::timeout_at(deadline, read).await {
        Ok(list) => list,
        Err(_) => {
            tracing::debug!(host = %crate::resolver::log_host(url), "a show's feed was too slow for its episodes");
            None
        }
    }
}

/// What names a feed item, for the list and for the resolver alike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemIdentity {
    /// The item's `<guid>`, trimmed; without one, its page; without either, `sha256:` and the
    /// hex digest of its enclosure's URL (without its fragment, as the `podcast:enclosure:` key
    /// digests it), so a link that carries a token names nothing of it.
    pub guid: String,
    /// Its `<link>`, resolved against the feed: the episode's own page.
    pub page: Option<String>,
    /// The URL of its first `<enclosure>`, resolved against the feed.
    pub enclosure: Option<String>,
}
impl ItemIdentity {
    /// The URI that resolves to the item's `PodcastEpisode` card: its page, else its feed item
    /// (`{feed}?guid={guid}`, which `resolver::cards::podcast_body` matches by this GUID).
    pub fn uri(&self, feed: &Url) -> String {
        self.page.clone().unwrap_or_else(|| {
            let mut u = feed.clone();
            u.query_pairs_mut().append_pair("guid", &self.guid);
            u.to_string()
        })
    }
}
/// A feed item's identity ([`ItemIdentity`]); `None` for an item nothing names (no GUID, no link,
/// no enclosure), which is no episode. One function names items for the episode list and for
/// the resolver, so every `uri` the list gives resolves to its episode's card.
pub fn item_identity(item: roxmltree::Node<'_, '_>, feed: &Url) -> Option<ItemIdentity> {
    let text = |name: &str| {
        item.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
            .and_then(|c| c.text())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    let link = |s: &str| crate::resolver::web::origin_url(feed, s).filter(|u| u.len() <= 2048);
    let enclosure = item
        .children()
        .find(|c| c.has_tag_name("enclosure") && c.attribute("url").is_some())
        .and_then(|e| e.attribute("url"))
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .and_then(link);
    // A relative link resolves against the feed's address: one that lands on a link carrying a
    // credential is no page.
    let page = text("link")
        .and_then(|l| link(&l))
        .filter(|u| !crate::capability::is_capability(u));
    let guid = text("guid")
        .filter(|g| g.len() <= 2048)
        .or_else(|| page.clone())
        .or_else(|| enclosure.as_deref().map(enclosure_digest))?;
    Some(ItemIdentity {
        guid,
        page,
        enclosure,
    })
}
/// `sha256:` and the hex digest of an enclosure's URL without its fragment.
fn enclosure_digest(url: &str) -> String {
    use sha2::Digest as _;
    let url = url.split_once('#').map_or(url, |(u, _)| u);
    format!("sha256:{:x}", sha2::Sha256::digest(url.as_bytes()))
}

fn episode(item: roxmltree::Node<'_, '_>, feed: &Url) -> Option<Episode> {
    let child = |name: &str| {
        item.children()
            .find(|c| c.is_element() && c.tag_name().name() == name)
    };
    let text = |name: &str| {
        child(name)
            .and_then(|c| c.text())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    // Resolved against the feed's address; never one that lands on a link carrying a credential.
    let link = |s: &str| {
        crate::resolver::web::origin_url(feed, s)
            .filter(|u| u.len() <= 2048 && !crate::capability::is_capability(u))
    };
    let identity = item_identity(item, feed)?;
    // An episode named by its feed item carries the feed's address in its link: none from a feed
    // read at an address that carries a credential.
    if identity.page.is_none() && crate::capability::is_capability_url(feed) {
        return None;
    }
    let uri = identity.uri(feed);
    if uri.len() > 2048 {
        return None;
    }
    Some(Episode {
        id: None,
        uri,
        name: text("title").map(|t| bounded(t, 512)),
        image: child("image")
            .and_then(|c| c.attribute("href"))
            .and_then(link),
        published: text("pubDate").as_deref().and_then(date),
        guid: identity.guid,
        // As the episode's own card reads it, so the list and the card play the same file.
        stream: crate::resolver::cards::item_stream(item),
    })
}
/// A date as feeds write it (RFC 2822, some RFC 3339), within the stored range.
fn date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s)
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .ok()
        .map(|d| d.to_utc())
        .and_then(crate::event::time::bounded)
}
fn bounded(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Writes a document's episodes as the document is written (`store::write_document`, so every
/// path that replaces a document passes here: a save, a clear): a list replaces the show's rows
/// whole; a document that is no podcast show, or a show found missing or cleared, clears them;
/// a show's document without a list (a record read without its feed) leaves them as they are.
pub(crate) async fn store(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    doc: &crate::model::ResourceMeta,
) -> Result<()> {
    let show = is_show(&doc.type_path, doc.status.as_str());
    match &doc.episodes {
        Some(list) if show => replace(tx, id, list).await,
        None if show => Ok(()),
        _ => clear(tx, id).await.map(|_| ()),
    }
}
/// Whether a card is a podcast show that has episodes to keep or to list: a `PodcastSeries`
/// whose status is `ok`. The one test for the store (which keeps or clears a card's rows by it)
/// and the route (which falls back to a group member's list only for such a card: a cleared or
/// missing card lists nothing, grouped or not).
pub(crate) fn is_show(type_path: &[String], status: &str) -> bool {
    type_path.iter().any(|t| t == "PodcastSeries") && status == "ok"
}
/// Clears a resource's episodes: for a document replaced outside a save (a scrub).
pub(crate) async fn clear(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM podcast_episode WHERE resource_id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?
            .rows_affected(),
    )
}
/// The table's bound on a stream's stored text (`podcast_episode_stream_check`); jsonb's text
/// form spaces its colons and commas, so the compact JSON is held a little under it.
const STREAM_TEXT_CAP: usize = 4096 - 64;
/// An episode's row as the table takes it, so no one episode fails the show's save: a stream
/// whose JSON exceeds the table's bound (a URL of 2,048 bytes that JSON escapes to twice that)
/// is stored as none; a name is cut to 512 bytes, an image or a date outside the bounds dropped;
/// an episode whose GUID or URI exceeds 2,048 bytes is not stored at all.
fn storable(e: &Episode) -> Option<(Episode, Option<String>)> {
    if e.guid.len() > 2048 || e.uri.len() > 2048 {
        return None;
    }
    let stream = e
        .stream
        .as_ref()
        .and_then(|s| serde_json::to_string(s).ok())
        .filter(|s| s.len() <= STREAM_TEXT_CAP);
    let mut row = e.clone();
    row.name = row.name.map(|n| bounded(n, 512));
    row.image = row.image.filter(|i| i.len() <= 2048);
    row.published = row.published.and_then(crate::event::time::bounded);
    row.stream = row.stream.filter(|_| stream.is_some());
    Some((row, stream))
}
async fn replace(tx: &mut Transaction<'_, Postgres>, id: &str, list: &[Episode]) -> Result<()> {
    clear(tx, id).await?;
    let list: Vec<(Episode, Option<String>)> =
        list.iter().filter_map(storable).take(KEPT).collect();
    if list.is_empty() {
        return Ok(());
    }
    let positions: Vec<i16> = (0..list.len() as i16).collect();
    let guids: Vec<&str> = list.iter().map(|(e, _)| e.guid.as_str()).collect();
    let urls: Vec<&str> = list.iter().map(|(e, _)| e.uri.as_str()).collect();
    let names: Vec<Option<&str>> = list.iter().map(|(e, _)| e.name.as_deref()).collect();
    let images: Vec<Option<&str>> = list.iter().map(|(e, _)| e.image.as_deref()).collect();
    let published: Vec<Option<DateTime<Utc>>> = list.iter().map(|(e, _)| e.published).collect();
    let streams: Vec<Option<&str>> = list.iter().map(|(_, s)| s.as_deref()).collect();
    sqlx::query("INSERT INTO podcast_episode(resource_id,position,guid,url,name,image,published,stream) SELECT $1,p,g,u,n,i,d,s::jsonb FROM unnest($2::smallint[],$3::text[],$4::text[],$5::text[],$6::text[],$7::timestamptz[],$8::text[]) AS t(p,g,u,n,i,d,s)")
        .bind(id).bind(&positions).bind(&guids).bind(&urls).bind(&names).bind(&images).bind(&published).bind(&streams)
        .execute(&mut **tx).await?;
    Ok(())
}

/// A stored episode's row with the resource it is listed for: that resource's ID, then the
/// episode's GUID, URI, name, image, date and stream (as JSON text).
type Row = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
);
#[derive(Deserialize, utoipa::IntoParams)]
pub struct EpisodeQuery {
    /// At most this many, newest first: 1–20, default 20.
    pub limit: Option<usize>,
}
#[derive(Serialize, ToSchema)]
pub struct EpisodePage {
    /// The show's latest episodes, newest first; empty for a resource that is no show or whose
    /// feed was not read yet.
    pub items: Vec<Episode>,
}
#[utoipa::path(get,path="/v1/resources/{id}/episodes",operation_id="list_episodes",tag="Media",params(("id"=String,Path),EpisodeQuery),responses((status=200,body=EpisodePage),(status=400,description="Invalid resource ID or limit"),(status=404,description="Resource not found")),description="A podcast show's latest episodes as its feed lists them, newest first: at most 20, each with its page or feed item, date and public stream. The show's own list, else, for a card that is a show, that of a member of its group which has one (an Apple Podcasts show grouped with its feed). A known resource that is no show, or no longer one, or whose feed was not read yet, returns an empty list.")]
pub async fn list(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<EpisodeQuery>,
) -> Result<axum::Json<EpisodePage>, crate::api::ApiError> {
    use crate::api::ApiError;
    if !crate::model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    let limit = q.limit.unwrap_or(KEPT);
    if !(1..=KEPT).contains(&limit) {
        return Err(ApiError::bad("limit must be 1–20"));
    }
    let root: Option<(String, Vec<String>, String)> = sqlx::query_as("SELECT r.id,coalesce(r.type_path,'{}'),r.status FROM resource r WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    let Some((root, type_path, status)) = root else {
        return Err(ApiError(
            axum::http::StatusCode::NOT_FOUND,
            "resource not found".into(),
        ));
    };
    // A group member's list stands in for a show's own only: a card cleared, missing or no show
    // lists nothing, whatever its group has. (`type_path` is null for an OpenStreetMap place,
    // whose card lives in `osm_place`: no show, read as an empty path.)
    let show = is_show(&type_path, &status);
    let mut items: Vec<Episode> = stored(&state.pool, &[(root, show)], limit, false)
        .await?
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    with_ids(&state.pool, items.iter_mut()).await?;
    Ok(axum::Json(EpisodePage { items }))
}

/// The most episodes a show's card carries (`Card::episodes`); the route lists up to [`KEPT`].
pub const ON_CARD: usize = 5;

/// Fills `episodes` on the cards that are podcast shows ([`is_show`]): the first [`ON_CARD`] of
/// the list the route gives, those with a `stream` alone (an item without public audio or video
/// is left out). One query for every show card of a response, one more when any has episodes;
/// none for a response without a show. A show with no such episode, or whose feed was not read
/// yet, gets none (the field stays absent).
pub(crate) async fn on_cards<'a>(
    pool: &sqlx::PgPool,
    cards: impl IntoIterator<Item = &'a mut crate::api::Card>,
) -> Result<(), crate::api::ApiError> {
    let mut shows: Vec<&mut crate::api::Card> = cards
        .into_iter()
        .filter(|c| c.occurrence.is_none() && is_show(&c.type_path, &c.status))
        .collect();
    if shows.is_empty() {
        return Ok(());
    }
    let ids: Vec<(String, bool)> = shows.iter().map(|c| (c.id.clone(), true)).collect();
    let mut lists: Vec<(String, Episode)> = stored(pool, &ids, ON_CARD, true).await?;
    with_ids(pool, lists.iter_mut().map(|(_, e)| e)).await?;
    for card in shows.iter_mut() {
        let list: Vec<Episode> = lists
            .iter()
            .filter(|(id, _)| *id == card.id)
            .map(|(_, e)| e.clone())
            .collect();
        card.episodes = (!list.is_empty()).then_some(list);
    }
    Ok(())
}

/// Each resource's stored list, in order, at most `limit` per resource, as `(resource ID,
/// episode)`: its own rows, else, when its flag is set (the resource is a show), those of the
/// member of its group whose newest episode is the newest. With `playable`, only that list's
/// episodes that have a `stream` (the list is chosen as without it, so a card never lists
/// another member's episodes than the route).
async fn stored(
    pool: &sqlx::PgPool,
    ids: &[(String, bool)],
    limit: usize,
    playable: bool,
) -> Result<Vec<(String, Episode)>, sqlx::Error> {
    let (ids, shows): (Vec<&str>, Vec<bool>) = ids.iter().map(|(i, s)| (i.as_str(), *s)).unzip();
    let rows: Vec<Row> = sqlx::query_as(
        "WITH asked AS (SELECT DISTINCT ON (id) id,borrow FROM unnest($1::text[],$2::bool[]) AS t(id,borrow) ORDER BY id,borrow DESC),\
         chosen AS (SELECT a.id AS card,coalesce(\
           (SELECT a.id WHERE EXISTS (SELECT 1 FROM podcast_episode e WHERE e.resource_id=a.id)),\
           (SELECT m.resource_id FROM group_unit u JOIN group_unit m ON m.group_id=u.group_id WHERE a.borrow AND u.unit_id=a.id AND u.group_id IS NOT NULL AND m.resource_id<>a.id AND m.occurrence_key='' AND EXISTS (SELECT 1 FROM podcast_episode e WHERE e.resource_id=m.resource_id) ORDER BY (SELECT max(published) FROM podcast_episode e WHERE e.resource_id=m.resource_id) DESC NULLS LAST,m.resource_id LIMIT 1)) AS source FROM asked a)\
         SELECT c.card,e.guid,e.url,e.name,e.image,e.published,e.stream FROM chosen c CROSS JOIN LATERAL \
           (SELECT e.position,e.guid,e.url,e.name,e.image,e.published,e.stream::text AS stream FROM podcast_episode e WHERE e.resource_id=c.source AND (NOT $4 OR e.stream IS NOT NULL) ORDER BY e.position LIMIT $3) e \
         ORDER BY c.card,e.position",
    )
    .bind(&ids)
    .bind(&shows)
    .bind(limit as i64)
    .bind(playable)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(card, guid, uri, name, image, published, stream)| {
            let stream: Option<Stream> = stream.and_then(|s| serde_json::from_str(&s).ok());
            (
                card,
                Episode {
                    id: None,
                    uri,
                    name,
                    image,
                    published,
                    guid,
                    stream,
                },
            )
        })
        .filter(|(_, e)| !playable || e.stream.is_some())
        .collect())
}

/// Names the episodes Pinakes has as resources (`id`), by their page's identity (an alias as its
/// root): one query.
async fn with_ids<'a>(
    pool: &sqlx::PgPool,
    episodes: impl IntoIterator<Item = &'a mut Episode>,
) -> Result<(), sqlx::Error> {
    let mut episodes: Vec<&mut Episode> = episodes.into_iter().collect();
    let identities: Vec<String> = episodes
        .iter()
        .filter_map(|e| crate::model::identity(&e.uri).ok().map(|v| v.0))
        .collect();
    if identities.is_empty() {
        return Ok(());
    }
    let known: Vec<(String, String)> = sqlx::query_as("SELECT i,r.id FROM unnest($1::text[]) i LEFT JOIN resource_alias a ON a.alias_id=i JOIN resource r ON r.id=coalesce(a.root_id,i)")
        .bind(&identities)
        .fetch_all(pool)
        .await?;
    for e in episodes.iter_mut() {
        let identity = crate::model::identity(&e.uri).ok().map(|v| v.0);
        e.id = known
            .iter()
            .find(|(i, _)| Some(i) == identity.as_ref())
            .map(|(_, id)| id.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(items: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" xmlns:podcast="https://podcastindex.org/namespace/1.0"><channel><title>Show</title>{items}</channel></rss>"#
        )
    }
    fn fetched(body: &str, truncated: bool) -> crate::resolver::egress::Fetched {
        crate::resolver::egress::Fetched {
            permanent_redirect: false,
            truncated,
            validators: Default::default(),
            url: "https://feeds.example.org/show.xml".parse().unwrap(),
            content_type: "application/rss+xml".into(),
            status: 200,
            retry_after_secs: 0,
            location: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// A feed read at an address that carries a credential lists only the episodes with a page of
    /// their own: one named by its feed item would carry that address in its public link.
    #[test]
    fn a_feed_that_carries_a_credential_lists_only_episodes_with_their_own_page() {
        let body = feed(concat!(
            r#"<item><title>Paged</title><guid>ep-1</guid><link>https://show.example/1</link><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Keyed</title><guid>ep-2</guid><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
        ));
        let mut f = fetched(&body, false);
        f.url = "https://feeds.example.org/show.xml?auth=SECRET"
            .parse()
            .unwrap();
        let episodes = from_feed(&f).unwrap();
        let uris: Vec<&str> = episodes.iter().map(|e| e.uri.as_str()).collect();
        assert_eq!(uris, ["https://show.example/1"]);
    }

    /// An item's relative link or image resolves against the feed's address: from a feed read at
    /// an address that carries a credential, one that lands on it is dropped, never listed.
    #[test]
    fn an_item_link_that_lands_on_a_credential_feed_is_dropped() {
        let body = feed(concat!(
            r#"<item><title>Anchored</title><guid>ep-2</guid><link>#ep2</link><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Pictured</title><guid>ep-3</guid><link>https://show.example/3</link><itunes:image href=""/><enclosure url="https://cdn.example/3.mp3" type="audio/mpeg"/></item>"#,
        ));
        let mut f = fetched(&body, false);
        f.url = "https://feeds.example.org/show.xml?auth=SECRET"
            .parse()
            .unwrap();
        let episodes = from_feed(&f).unwrap();
        assert!(!serde_json::to_string(&episodes).unwrap().contains("SECRET"));
        let uris: Vec<&str> = episodes.iter().map(|e| e.uri.as_str()).collect();
        assert_eq!(uris, ["https://show.example/3"]);
    }

    #[test]
    fn a_feeds_items_are_its_latest_episodes_newest_first_with_their_public_streams() {
        let body = feed(concat!(
            // Out of order: the second item is the newest.
            r#"<item><title>Two</title><guid isPermaLink="false">ep-2</guid><link>https://show.example/2</link><pubDate>Tue, 20 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>59:17</itunes:duration><itunes:image href="/art/2.jpg"/><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg" length="100"/></item>"#,
            r#"<item><title>Three</title><guid>ep-3</guid><link>https://show.example/3</link><pubDate>Wed, 21 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>1:02:03</itunes:duration><enclosure url="https://cdn.example/3.mp3" type="audio/mpeg"/></item>"#,
            // A signed enclosure is listed without a stream.
            r#"<item><title>One</title><guid>ep-1</guid><link>https://show.example/1</link><pubDate>Mon, 19 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>3557</itunes:duration><enclosure url="https://cdn.example/1.mp3?Expires=1&amp;Signature=abc" type="audio/mpeg"/></item>"#,
            // No GUID and no link: a digest of the enclosure names it, and its feed item is its URI.
            r#"<item><title>Zero</title><enclosure url="https://cdn.example/0.m4a#t=1" type="audio/mp4"/></item>"#,
            // Not an episode: nothing names it.
            r#"<item><title>Note</title><description>text</description></item>"#,
        ));
        let episodes = from_feed(&fetched(&body, false)).unwrap();
        assert_eq!(
            episodes
                .iter()
                .map(|e| e.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["Three", "Two", "One", "Zero"]
        );
        let three = &episodes[0];
        assert_eq!(three.uri, "https://show.example/3");
        assert_eq!(three.guid, "ep-3");
        assert_eq!(
            three.stream,
            Some(Stream {
                url: "https://cdn.example/3.mp3".into(),
                media_type: "audio/mpeg".into(),
                duration_s: Some(3723),
                preview: false
            })
        );
        let two = &episodes[1];
        assert_eq!(two.stream.as_ref().unwrap().duration_s, Some(3557));
        assert_eq!(
            two.image.as_deref(),
            Some("https://feeds.example.org/art/2.jpg")
        );
        assert_eq!(
            two.published.unwrap().to_rfc3339(),
            "2026-10-20T10:00:00+00:00"
        );
        assert_eq!(episodes[2].stream, None, "a signed enclosure is no stream");
        let zero = &episodes[3];
        let digest = format!("sha256:{:x}", {
            use sha2::Digest as _;
            sha2::Sha256::digest(b"https://cdn.example/0.m4a")
        });
        assert_eq!(zero.guid, digest, "the fragment is not part of the name");
        assert_eq!(
            zero.uri,
            format!(
                "https://feeds.example.org/show.xml?guid={}",
                digest.replace(':', "%3A")
            )
        );
        assert_eq!(zero.published, None);
        assert_eq!(
            zero.stream
                .as_ref()
                .map(|s| (s.media_type.as_str(), s.duration_s)),
            Some(("audio/mp4", None))
        );
        // An enclosure without a type is no stream, as the episode's own card has none.
        let untyped = feed(
            r#"<item><title>Untyped</title><guid>u</guid><enclosure url="https://cdn.example/u.mp3"/></item>"#,
        );
        let listed = from_feed(&fetched(&untyped, false)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].stream, None);
        // A feed cut at the cap inside its last item reads the whole ones.
        let cut = &body[..body.find("<item><title>Note").unwrap() + 20];
        assert_eq!(from_feed(&fetched(cut, true)).unwrap().len(), 4);
        assert_eq!(from_feed(&fetched("<html></html>", false)), None);
    }

    /// An item named only by an enclosure that is no public stream (a signed link): listed,
    /// without a stream, and nothing of the link in its name or URI.
    #[test]
    fn a_rejected_enclosure_names_its_item_by_a_digest_only() {
        let body = feed(
            r#"<item><title>Hidden</title><enclosure url="https://cdn.example/ep.mp3?token=SECRET" type="audio/mpeg"/></item>"#,
        );
        let episodes = from_feed(&fetched(&body, false)).unwrap();
        assert_eq!(episodes.len(), 1);
        let e = &episodes[0];
        assert_eq!(e.stream, None);
        assert!(e.guid.starts_with("sha256:"), "{e:?}");
        let listed = serde_json::to_string(&episodes).unwrap();
        assert!(!listed.contains("SECRET"), "{listed}");
        assert!(!listed.contains("cdn.example"), "{listed}");
    }

    /// Every URI the list gives resolves to its item's `PodcastEpisode` card, named by the same
    /// GUID: an item with a GUID and a page, one with a page only, one with an enclosure only,
    /// one whose enclosure is no stream.
    #[test]
    fn every_listed_uri_resolves_to_its_episode() {
        let body = feed(concat!(
            r#"<item><title>Named</title><guid> ep-1 </guid><link>https://show.example/1</link><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Paged</title><link>/2</link><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Filed</title><enclosure url="https://cdn.example/3.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Hidden</title><enclosure url="https://cdn.example/4.mp3?token=SECRET" type="audio/mpeg"/></item>"#,
            r#"<item><title>Keyed</title><guid>ep-5</guid><enclosure url="https://cdn.example/5.mp3" type="audio/mpeg"/></item>"#,
        ));
        let f = fetched(&body, false);
        let episodes = from_feed(&f).unwrap();
        assert_eq!(episodes.len(), 5);
        assert_eq!(episodes[0].guid, "ep-1", "trimmed");
        assert_eq!(episodes[1].uri, "https://feeds.example.org/2");
        assert_eq!(episodes[1].guid, "https://feeds.example.org/2");
        assert_eq!(
            episodes[4].uri,
            "https://feeds.example.org/show.xml?guid=ep-5"
        );
        for e in &episodes {
            // The resolver reads the feed for the URI: the page's feed, or the feed item's URL
            // (the feed itself, whatever the query).
            let card = crate::resolver::cards::podcast(&e.uri, &f, 3600, 1000)
                .unwrap()
                .unwrap_or_else(|| panic!("{} resolves to nothing", e.uri));
            assert_eq!(card.type_name, "PodcastEpisode", "{}", e.uri);
            assert_eq!(card.status, crate::model::Status::Ok, "{}", e.uri);
            assert_eq!(
                card.attributes.get("podcast_guid"),
                Some(&e.guid),
                "{}",
                e.uri
            );
            assert_eq!(card.name, e.name);
        }
        // A GUID the feed does not have is missing, as before.
        let card = crate::resolver::cards::podcast(
            "https://feeds.example.org/show.xml?guid=ep-9",
            &f,
            3600,
            1000,
        )
        .unwrap()
        .unwrap();
        assert_eq!(card.status, crate::model::Status::Missing);
    }

    #[test]
    fn the_latest_twenty_are_kept() {
        let items: String = (1..=30)
            .map(|n| format!(r#"<item><title>E{n}</title><guid>e{n}</guid><pubDate>{}</pubDate><enclosure url="https://cdn.example/{n}.mp3" type="audio/mpeg"/></item>"#, chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + n * 86_400, 0).unwrap().to_rfc2822()))
            .collect();
        let episodes = from_feed(&fetched(&feed(&items), false)).unwrap();
        assert_eq!(episodes.len(), KEPT);
        assert_eq!(episodes[0].name.as_deref(), Some("E30"));
        assert_eq!(episodes[19].name.as_deref(), Some("E11"));
    }

    #[test]
    fn dates_as_feeds_write_them() {
        assert!(date("Tue, 20 Oct 2026 10:00:00 GMT").is_some());
        assert!(date("2026-10-20T10:00:00Z").is_some());
        assert!(date("yesterday").is_none());
    }

    /// The rules of PLAN-podcasts.md: public, direct audio, nothing signed, expiring or behind a
    /// login.
    #[test]
    fn a_stream_is_public_direct_audio() {
        let ok = |u: &str| Stream::public(u, "audio/mpeg", Some(10), false).is_some();
        assert!(ok("https://cdn.example/a.mp3"));
        assert!(ok("http://cdn.example/a.mp3"));
        assert!(ok(
            "https://dts.podtrac.com/redirect.mp3/cdn.example/a.mp3?ref=x"
        ));
        assert!(!ok("https://cdn.example/a.mp3?Expires=1"));
        assert!(!ok("https://cdn.example/a.mp3?X-Amz-Signature=1"));
        assert!(!ok("https://cdn.example/a.mp3?x-goog-expires=1"));
        assert!(!ok("https://cdn.example/a.mp3?hdnea=exp=1~hmac=2"));
        assert!(!ok("https://cdn.example/a.mp3?token=abc"));
        assert!(!ok("https://cdn.example/a.mp3?Policy=p&Key-Pair-Id=k"));
        assert!(!ok("https://user:pw@cdn.example/a.mp3"));
        assert!(!ok("https://10.0.0.1/a.mp3"));
        assert!(!ok("https://[::1]/a.mp3"));
        assert!(!ok("https://localhost/a.mp3"));
        assert!(!ok("https://cdn/a.mp3"));
        assert!(!ok("ftp://cdn.example/a.mp3"));
        assert!(!ok(&format!(
            "https://cdn.example/{}.mp3",
            "x".repeat(2048)
        )));
        assert!(Stream::public("https://cdn.example/a", "text/html", None, false).is_none());
        assert!(Stream::public("https://cdn.example/a", "audio/", None, false).is_none());
        assert_eq!(
            Stream::public(
                "https://cdn.example/a",
                " Video/MP4; codecs=avc1",
                None,
                true
            )
            .unwrap()
            .media_type,
            "video/mp4"
        );
        assert!(Stream::public(
            "https://cdn.example/a.mp3",
            "audio/mpeg",
            Some(8 * 86_400),
            false
        )
        .is_none());
    }
}
