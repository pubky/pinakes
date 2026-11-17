//! A podcast show's latest episodes, so a client can list and play them from the show's card.
//! They are read from the show's feed as the show's card is built (the
//! feed's own card in `resolver::cards::podcast_body`; a Podcast Index or Apple Podcasts show's
//! card fetches the feed its record names), kept in `podcast_episode` (the latest [`KEPT`],
//! replaced whole at each save of the show, so they refresh with it) and served by
//! `GET /v1/resources/{id}/episodes` (docs/api.md, Episodes); every show card carries the first
//! [`ON_CARD`] that have a stream (`Card::episodes`, filled by [`on_cards`] from the stored rows).
//!
//! Why a route and a table, not a fact: a fact is one string of at most 512 bytes per name and a
//! card's document is bounded at 8 KiB; a hundred episodes with their streams are neither. Why
//! not occurrences: an episode has no instant to window by, no recurrence and no document of its
//! own here; the list is the feed's head and nothing more. An entry is not a card: the episode's
//! card is its `uri` (its feed item) resolved (`POST /v1/resolve`), and `id` names that card when
//! Pinakes has it. That card names its show (`related`, `show`), and its page joins it as an
//! alias when the feed's own publisher publishes the page ([`join_page`]).
use crate::model::Stream;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use std::time::Duration;
use url::Url;
use utoipa::ToSchema;

/// How many of a show's latest episodes are kept, and paged through on the route: the table's
/// bound (`podcast_episode_position_check`).
pub const KEPT: usize = 100;
/// The most episodes one page of the route lists.
pub const PAGE: usize = 20;
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
    /// The episode's feed item (`{feed}?guid={guid}`), which `POST /v1/resolve` reads as its
    /// `PodcastEpisode` card from the feed ([`ItemIdentity::uri`]); from a feed whose address
    /// carries a credential, the item's page.
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
    /// Its `<link>`, resolved against the feed and without its tracking parameters (`utm_*`,
    /// `fbclid`, …: `alias::untracked`): the episode's own page.
    pub page: Option<String>,
    /// The URL of its first `<enclosure>`, resolved against the feed.
    pub enclosure: Option<String>,
}
impl ItemIdentity {
    /// The URI the list gives the item: its feed item ([`item_uri`], which
    /// `resolver::cards::podcast_body` matches by this GUID), the one form that reads as the
    /// item's `PodcastEpisode` card from the feed alone (a page may read as an article: a blog
    /// post's markup, no feed named). From a feed read at an address that carries a credential,
    /// its page instead, since a feed item would carry that address; `None` without one.
    pub fn uri(&self, feed: &Url) -> Option<String> {
        if crate::capability::is_capability_url(feed) {
            return self.page.clone();
        }
        Some(item_uri(feed, &self.guid))
    }
}
/// A feed item as a URI: the feed's address with its `guid` (`{feed}?guid={guid}`), any `guid`
/// the address held replaced.
pub fn item_uri(feed: &Url, guid: &str) -> String {
    let mut u = show_feed(feed);
    u.query_pairs_mut().append_pair("guid", guid);
    u.to_string()
}
/// The feed an address names: the address without its `guid` query parameters (a feed item's,
/// [`item_uri`]). The other parameters keep their own encoding.
pub fn show_feed(url: &Url) -> Url {
    let mut u = url.clone();
    if let Some(query) = url.query() {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|piece| {
                let name = piece.split('=').next().unwrap_or_default();
                percent_encoding::percent_decode_str(name).decode_utf8_lossy() != "guid"
            })
            .collect();
        let kept = kept.join("&");
        u.set_query((!kept.is_empty()).then_some(kept.as_str()));
    }
    u
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
    // credential is no page. A page shared with tracking parameters is the same page.
    let page = text("link")
        .and_then(|l| link(&l))
        .filter(|u| !crate::capability::is_capability(u))
        .map(|u| crate::alias::untracked(&u).unwrap_or(u));
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
/// Whether `page`, the link of an item of the feed read at `feed` (its `channel` and the `items`
/// read), is that one item's own page: no other item read links it, and it is not the feed's
/// own `<link>` (the show's site), a site's root, nor a feed or a feed's item ([`a_feed`]). A
/// page several items link is the show's, never one episode's.
pub(crate) fn own_page(
    page: &str,
    items: &[roxmltree::Node<'_, '_>],
    channel: roxmltree::Node<'_, '_>,
    feed: &Url,
) -> bool {
    let id = |u: &str| crate::model::identity(u).ok().map(|v| v.0);
    let Some(own) = id(page) else {
        return false;
    };
    if site_root(page) || a_feed(page, feed) {
        return false;
    }
    let site = channel
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "link")
        .find_map(|c| c.text())
        .map(str::trim)
        .and_then(|l| crate::resolver::web::origin_url(feed, l))
        .map(|u| crate::alias::untracked(&u).unwrap_or(u));
    if site.as_deref().and_then(id).as_ref() == Some(&own) {
        return false;
    }
    items
        .iter()
        .filter(|n| {
            item_identity(**n, feed)
                .and_then(|i| i.page)
                .and_then(|p| id(&p))
                .as_ref()
                == Some(&own)
        })
        .count()
        == 1
}
/// Whether `page` is a feed's own address or a feed item's: it names an item (a `guid`
/// parameter), or it is the feed `feed` names (`feed` itself, or another of its items).
fn a_feed(page: &str, feed: &Url) -> bool {
    let Ok(url) = Url::parse(page) else {
        return true;
    };
    let id = |u: &Url| {
        crate::model::identity(show_feed(u).as_str())
            .ok()
            .map(|v| v.0)
    };
    url.query_pairs().any(|(k, _)| k == "guid") || id(&url) == id(feed)
}
/// A site's root: a web address whose path is empty or `/`, without a query.
fn site_root(page: &str) -> bool {
    Url::parse(page).is_ok_and(|u| matches!(u.path(), "" | "/") && u.query().is_none())
}
/// Whether two web addresses' hosts are one, or one is a subdomain of the other
/// (`podcast.example.com` and `example.com`).
fn related_hosts(a: &str, b: &str) -> bool {
    let host = |u: &str| {
        Url::parse(u).ok().and_then(|u| {
            u.host_str()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        })
    };
    let (Some(a), Some(b)) = (host(a), host(b)) else {
        return false;
    };
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
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
    // A feed item carries the feed's address in its link: from a feed read at an address that
    // carries a credential, only an item with a page of its own is listed, by that page.
    let uri = identity.uri(feed)?;
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
/// is stored as none; a name is cut to 512 bytes, an image outside the bounds or a web link not
/// on a named public host (`model::private_web_link`) dropped, and a date outside the bounds;
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
    row.image = row
        .image
        .filter(|i| i.len() <= 2048 && !crate::model::private_web_link(i));
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
    /// How many of the matching episodes to skip, newest first: 0–100, default 0 (a page's
    /// `next_offset` gives the next page's).
    pub offset: Option<usize>,
    /// Only the episodes whose name holds this text, in any letter case; at most 256 bytes,
    /// spaces around it ignored, a blank one matching every episode.
    pub q: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct EpisodePage {
    /// The show's latest episodes, newest first; empty for a resource that is no show or whose
    /// feed was not read yet.
    pub items: Vec<Episode>,
    /// The `offset` of the next page when more episodes match; null on the last page.
    pub next_offset: Option<usize>,
}
#[utoipa::path(get,path="/v1/resources/{id}/episodes",operation_id="list_episodes",tag="Media",params(("id"=String,Path),EpisodeQuery),responses((status=200,body=EpisodePage),(status=400,description="Invalid resource ID, limit, offset or q"),(status=404,description="Resource not found")),description="A podcast show's latest episodes as its feed lists them, newest first: the latest 100, in pages of at most 20 (`offset`, `next_offset`), optionally only those whose name holds `q` (any letter case), each with its feed item (which resolves to its PodcastEpisode card), date and public stream. The show's own list, else, for a card that is a show, that of a member of its group which has one (an Apple Podcasts show grouped with its feed). A known resource that is no show, or no longer one, or whose feed was not read yet, returns an empty list.")]
pub async fn list(
    axum::extract::State(state): axum::extract::State<crate::api::AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<EpisodeQuery>,
) -> Result<axum::Json<EpisodePage>, crate::api::ApiError> {
    use crate::api::ApiError;
    if !crate::model::valid_id(&id) {
        return Err(ApiError::bad("invalid resource ID"));
    }
    let limit = q.limit.unwrap_or(PAGE);
    if !(1..=PAGE).contains(&limit) {
        return Err(ApiError::bad("limit must be 1–20"));
    }
    let offset = q.offset.unwrap_or(0);
    if offset > KEPT {
        return Err(ApiError::bad("offset must be 0–100"));
    }
    if q.q.as_ref().is_some_and(|t| t.len() > 256) {
        return Err(ApiError::bad("q is at most 256 bytes"));
    }
    // Folded as Rust folds letter case (`str::to_lowercase`), whatever the database's locale.
    let wanted =
        q.q.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_lowercase);
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
    // The whole stored list (at most `KEPT` rows), matched and paged here.
    let mut items: Vec<Episode> = stored(&state.pool, &[(root, show)], KEPT, false)
        .await?
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| {
            wanted.as_deref().is_none_or(|w| {
                e.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(w))
            })
        })
        .skip(offset)
        .take(limit + 1)
        .collect();
    let next_offset = (items.len() > limit).then(|| {
        items.truncate(limit);
        offset + limit
    });
    with_ids(&state.pool, items.iter_mut()).await?;
    Ok(axum::Json(EpisodePage { items, next_offset }))
}

/// The most episodes a show's card carries (`Card::episodes`); the route pages through [`KEPT`].
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

/// The types of a page that may join an episode's card: a page read as a plain web page or an
/// article (a blog post's markup). A page read as an episode joins only when it names the same
/// feed (`podcast_feed`).
const JOINABLE: &[&str] = &[
    "Thing",
    "WebPage",
    "CreativeWork",
    "Article",
    "BlogPosting",
    "NewsArticle",
];
/// Makes an episode's page an alias of the episode's card, read from its feed item (`uri`, the
/// card `id`, its job's `generation`) that a stored show list names: the feed names the page as
/// the item's own link (`ResourceMeta::episode_page`, [`own_page`]), and the feed's own publisher
/// publishes the page: one registrable domain, or one tenant of a shared host
/// (`group_keys::same_origin`), on one host or a subdomain of the other. So a page that reads as
/// an article (a blog post's markup that names no feed) answers as the episode, and its tracking
/// spellings, already its aliases, follow it. Only a page Pinakes has read joins, and only while
/// it is a plain page of its own: no group, no aliases but its tracking spellings (which
/// [`release_pages`] can give back), read as a type of [`JOINABLE`], or as an episode of the same
/// feed. A page not read yet, a
/// page that is already an alias, a site's root, a feed or a feed's item, or a page a rule of its
/// URI gives another root (`alias::root`), is left as it is: a page read later joins at the
/// item's next read. The basis is `source`, the feed's own claim: no rule of the URI names it, so
/// `reextract` keeps it. Nothing joins when the card is an alias itself or its job is stale (the
/// generation fence).
pub(crate) async fn join_page(
    pool: &sqlx::PgPool,
    id: &str,
    generation: i64,
    uri: &str,
    page: &str,
) -> Result<()> {
    let (Ok((page_id, page_uri, scheme)), Ok(feed)) =
        (crate::model::identity(page), Url::parse(uri))
    else {
        return Ok(());
    };
    if page_id == id
        || !matches!(scheme.as_str(), "http" | "https")
        || page_uri.len() > 2048
        || crate::capability::is_capability(&page_uri)
        || crate::capability::is_capability(uri)
        || !crate::group_keys::same_origin(&page_uri, uri)
        || !related_hosts(&page_uri, uri)
        || site_root(&page_uri)
        || a_feed(&page_uri, &feed)
        || crate::alias::root(&page_uri).is_some()
    {
        return Ok(());
    }
    let feed_id = crate::model::identity(show_feed(&feed).as_str())
        .ok()
        .map(|v| v.0);
    // An item a show's stored list names, not one of any feed someone asked for (a site's search
    // or category feed lists its posts too).
    let listed: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM podcast_episode WHERE url=$1)")
            .bind(uri)
            .fetch_one(pool)
            .await?;
    if !listed {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    crate::alias::lock(&mut tx).await?;
    // The alias's row first, then its root's, as `alias::assign` locks them. A page not read yet
    // is left: its address alone says nothing of what it is.
    let known: Option<String> =
        sqlx::query_scalar("SELECT id FROM resource WHERE id=$1 FOR UPDATE")
            .bind(&page_id)
            .fetch_optional(&mut *tx)
            .await?;
    if known.is_none() {
        return Ok(());
    }
    // An alias already, or no plain page of its own: left as it is.
    let (aliased, spellings, grouped, type_path, page_feed): (
        bool,
        bool,
        bool,
        Option<Vec<String>>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM resource_alias WHERE alias_id=$1),\
         EXISTS(SELECT 1 FROM resource_alias WHERE root_id=$1 AND basis<>'tracking'),\
         EXISTS(SELECT 1 FROM group_unit WHERE resource_id=$1 AND group_id IS NOT NULL),\
         (SELECT type_path FROM resource WHERE id=$1),\
         (SELECT doc->'attributes'->>'podcast_feed' FROM resource WHERE id=$1)",
    )
    .bind(&page_id)
    .fetch_one(&mut *tx)
    .await?;
    let same_feed = || {
        page_feed
            .as_deref()
            .and_then(|f| Url::parse(f).ok())
            .and_then(|f| crate::model::identity(show_feed(&f).as_str()).ok())
            .map(|v| v.0)
            .is_some_and(|f| Some(f) == feed_id)
    };
    let joinable = match type_path.as_deref().and_then(<[String]>::last) {
        Some(t) if t == "PodcastEpisode" => same_feed(),
        Some(t) => JOINABLE.contains(&t.as_str()),
        None => false,
    };
    if aliased || spellings || grouped || !joinable {
        return Ok(());
    }
    let current: Option<String> = sqlx::query_scalar(
        "SELECT id FROM resource r WHERE id=$1 AND generation=$2 AND NOT EXISTS (SELECT 1 FROM resource_alias a WHERE a.alias_id=r.id) FOR UPDATE",
    )
    .bind(id)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?;
    if current.is_none() {
        // A stale job, or a card that became an alias: nothing joins.
        return Ok(());
    }
    let alias = crate::alias::Alias {
        root: uri.to_owned(),
        basis: "source",
    };
    let line = crate::alias::assign(&mut tx, &page_id, &alias).await?;
    crate::alias::record(&mut tx, line.into_iter().collect()).await?;
    tx.commit().await?;
    Ok(())
}

/// Lets go of the pages joined to an episode's card ([`join_page`], basis `source`) that its feed
/// item no longer names as its own: those other than `keep`, the page it names now (every one
/// when it names none). When the item has left the feed (`gone`), only once the card is
/// confirmed missing (its stored status), so one stray answer moves nothing. Each answers for
/// itself again, with the tracking spellings that followed it, and is read again on its own
/// route (`alias::release`). Nothing moves when the job is stale (the generation fence).
pub(crate) async fn release_pages(
    pool: &sqlx::PgPool,
    id: &str,
    generation: i64,
    keep: Option<&str>,
    gone: bool,
) -> Result<()> {
    let keep = keep
        .and_then(|p| crate::model::identity(p).ok())
        .map(|v| v.0);
    let joined: Vec<String> = sqlx::query_scalar(
        "SELECT alias_id FROM resource_alias WHERE root_id=$1 AND basis='source' AND ($2::text IS NULL OR alias_id<>$2)",
    )
    .bind(id)
    .bind(&keep)
    .fetch_all(pool)
    .await?;
    if joined.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    crate::store::lock_writers(&mut tx, crate::store::WriteOrder::Shared).await?;
    crate::alias::lock(&mut tx).await?;
    // The aliases' rows first (the pages, and the spellings that followed them here), then their
    // root's, as `alias::assign` locks them.
    let pages: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.id,r.uri FROM resource r JOIN resource_alias a ON a.alias_id=r.id WHERE r.id=ANY($1) AND a.root_id=$2 AND a.basis='source' ORDER BY r.id FOR UPDATE OF r",
    )
    .bind(&joined)
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let spellings: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.id,r.uri FROM resource r JOIN resource_alias a ON a.alias_id=r.id WHERE a.root_id=$1 AND a.basis<>'source' ORDER BY r.id FOR UPDATE OF r",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let current: Option<String> =
        sqlx::query_scalar("SELECT status FROM resource WHERE id=$1 AND generation=$2 FOR UPDATE")
            .bind(id)
            .bind(generation)
            .fetch_optional(&mut *tx)
            .await?;
    let settled = current
        .as_deref()
        .is_some_and(|status| !gone || status == "missing");
    if pages.is_empty() || !settled {
        return Ok(());
    }
    let mut lines = vec![];
    for (page, uri) in &pages {
        lines.extend(crate::alias::release(&mut tx, page).await?);
        // Read again on its own route, now: it answers for itself.
        sqlx::query("UPDATE resource SET resolver=$2,attempts=0,timeout_streak=0,dead_letter=false,final=false,last_error=NULL,lease_until=NULL,priority_at=now(),generation=generation+1,next_attempt_at=now() WHERE id=$1")
            .bind(page)
            .bind(crate::resolver::classify(uri))
            .execute(&mut *tx)
            .await?;
    }
    // A page's own spellings (its tracking parameters) followed it to this card: back to it.
    let released: Vec<&str> = pages.iter().map(|(p, _)| p.as_str()).collect();
    for (spelling, uri) in &spellings {
        if let Some(alias) = crate::alias::root(uri).filter(|a| {
            crate::model::identity(&a.root).is_ok_and(|v| released.contains(&v.0.as_str()))
        }) {
            lines.extend(crate::alias::assign(&mut tx, spelling, &alias).await?);
        }
    }
    crate::alias::record(&mut tx, lines).await?;
    tx.commit().await?;
    Ok(())
}
/// Whether `uri` is a feed item's (`{feed}?guid={guid}`, [`item_uri`]).
pub(crate) fn names_an_item(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|u| u.query_pairs().any(|(k, _)| k == "guid"))
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
            truncated,
            retry_after_secs: 0,
            ..crate::resolver::egress::Fetched::answer(
                "https://feeds.example.org/show.xml".parse().unwrap(),
                200,
                "application/rss+xml".into(),
                body.as_bytes().to_vec(),
            )
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
        assert_eq!(three.uri, "https://feeds.example.org/show.xml?guid=ep-3");
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
        assert_eq!(
            episodes[0].uri, "https://feeds.example.org/show.xml?guid=ep-1",
            "an item with a page is named by its feed item too"
        );
        assert_eq!(
            episodes[1].uri,
            "https://feeds.example.org/show.xml?guid=https%3A%2F%2Ffeeds.example.org%2F2"
        );
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

    /// The latest hundred of the first 200 items: the items past them are not looked at.
    #[test]
    fn the_latest_hundred_of_the_first_two_hundred_are_kept() {
        let items: String = (1..=230)
            .map(|n| format!(r#"<item><title>E{n}</title><guid>e{n}</guid><pubDate>{}</pubDate><enclosure url="https://cdn.example/{n}.mp3" type="audio/mpeg"/></item>"#, chrono::DateTime::<Utc>::from_timestamp(1_700_000_000 + n * 86_400, 0).unwrap().to_rfc2822()))
            .collect();
        let episodes = from_feed(&fetched(&feed(&items), false)).unwrap();
        assert_eq!(episodes.len(), KEPT);
        assert_eq!(KEPT, 100);
        // Items 201-230 are past the part of the feed that is read: E200 is the newest seen.
        assert_eq!(episodes[0].name.as_deref(), Some("E200"));
        assert_eq!(episodes[99].name.as_deref(), Some("E101"));
    }

    /// A page shared with tracking parameters is the same page: the item's page, and the GUID of
    /// an item named by its page, carry none of them; its other parameters stay as written.
    #[test]
    fn an_items_page_carries_no_tracking_parameters() {
        let body = feed(concat!(
            r#"<item><title>One</title><guid>https://show.example/?p=1</guid><link>https://show.example/one/?utm_source=rss&amp;utm_medium=rss&amp;utm_campaign=one</link><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item>"#,
            r#"<item><title>Two</title><link>https://show.example/two?id=2&amp;fbclid=x</link><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg"/></item>"#,
        ));
        let f = fetched(&body, false);
        let xml = roxmltree::Document::parse(&body).unwrap();
        let items: Vec<_> = xml
            .descendants()
            .filter(|n| n.has_tag_name("item"))
            .collect();
        let one = item_identity(items[0], &f.url).unwrap();
        assert_eq!(one.page.as_deref(), Some("https://show.example/one/"));
        assert_eq!(one.guid, "https://show.example/?p=1");
        let two = item_identity(items[1], &f.url).unwrap();
        assert_eq!(two.page.as_deref(), Some("https://show.example/two?id=2"));
        assert_eq!(two.guid, "https://show.example/two?id=2");
        let listed = serde_json::to_string(&from_feed(&f).unwrap()).unwrap();
        assert!(!listed.contains("utm_"), "{listed}");
        assert!(!listed.contains("fbclid"), "{listed}");
    }

    /// A feed item's URI: the feed's address with the item's `guid`, a `guid` the address held
    /// replaced and its other parameters kept as written; the show is the address without it.
    #[test]
    fn a_feed_items_uri_and_its_show() {
        let feed: Url = "https://f.example/rss?a=b%20c&guid=old&x".parse().unwrap();
        assert_eq!(show_feed(&feed).as_str(), "https://f.example/rss?a=b%20c&x");
        assert_eq!(
            item_uri(&feed, "ep 1/2"),
            "https://f.example/rss?a=b%20c&x&guid=ep+1%2F2"
        );
        let plain: Url = "https://f.example/rss?guid=a".parse().unwrap();
        assert_eq!(show_feed(&plain).as_str(), "https://f.example/rss");
        assert_eq!(item_uri(&plain, "b"), "https://f.example/rss?guid=b");
    }

    /// An episode read from its feed item names its show (`related`, `show`: the feed, by its
    /// title) and the page its item links, without tracking, to join as an alias; read where a
    /// redirect led, or from a feed whose address carries a credential, it names no page to join,
    /// and from the latter no show either.
    #[test]
    fn an_episode_names_its_show_and_its_page() {
        let body = feed(
            r#"<item><title>One</title><guid>ep-1</guid><link>https://show.example/1/?utm_source=rss</link><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item>"#,
        );
        let mut f = fetched(&body, false);
        f.url = "https://feeds.example.org/show.xml?guid=ep-1"
            .parse()
            .unwrap();
        let card = crate::resolver::cards::podcast(f.url.as_str(), &f, 3600, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "PodcastEpisode");
        let mut show =
            crate::model::Related::new("show", Some("https://feeds.example.org/show.xml".into()));
        show.name = Some("Show".into());
        assert_eq!(card.related, [show]);
        assert_eq!(
            card.episode_page.as_deref(),
            Some("https://show.example/1/")
        );
        assert!(card.episode_item);
        // Asked at another address (a redirect led to the feed): the show, no page to join.
        let card =
            crate::resolver::cards::podcast("https://old.example/feed?guid=ep-1", &f, 3600, 1000)
                .unwrap()
                .unwrap();
        assert_eq!(card.related.len(), 1);
        assert_eq!(card.episode_page, None);
        assert!(
            card.episode_item,
            "a redirected read still settles the joined pages"
        );
        // A feed whose address carries a credential: neither.
        f.url = "https://feeds.example.org/show.xml?token=SECRET&guid=ep-1"
            .parse()
            .unwrap();
        let card = crate::resolver::cards::podcast(f.url.as_str(), &f, 3600, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(card.type_name, "PodcastEpisode");
        assert!(card.related.is_empty(), "{:?}", card.related);
        assert_eq!(card.episode_page, None);
        assert!(!card.episode_item);
        // A show's name is cut to the entry's bound.
        let long = format!(
            r#"<?xml version="1.0"?><rss version="2.0"><channel><title>{}</title><item><title>One</title><guid>ep-1</guid><enclosure url="https://cdn.example/1.mp3" type="audio/mpeg"/></item></channel></rss>"#,
            "é".repeat(100)
        );
        let mut f = fetched(&long, false);
        f.url = "https://feeds.example.org/show.xml?guid=ep-1"
            .parse()
            .unwrap();
        let card = crate::resolver::cards::podcast(f.url.as_str(), &f, 3600, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(card.related[0].name.as_ref().unwrap().len(), 128);
    }

    /// Only an item's own page joins its episode: not one several items link, the show's site
    /// (the channel's `<link>`), a site's root, the feed itself or a feed's item.
    #[test]
    fn only_an_items_own_page_joins_its_episode() {
        let body = format!(
            r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Show</title><link>https://show.example/podcast/?utm_source=x</link>{}</channel></rss>"#,
            concat!(
                r#"<item><title>A</title><guid>a</guid><link>https://show.example/a/</link><enclosure url="https://cdn.example/a.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>B</title><guid>b</guid><link>https://show.example/shared/</link><enclosure url="https://cdn.example/b.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>C</title><guid>c</guid><link>https://show.example/shared/?utm_medium=rss</link><enclosure url="https://cdn.example/c.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>D</title><guid>d</guid><link>https://show.example/podcast/</link><enclosure url="https://cdn.example/d.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>E</title><guid>e</guid><link>https://show.example/</link><enclosure url="https://cdn.example/e.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>F</title><guid>f</guid><link>https://feeds.example.org/show.xml</link><enclosure url="https://cdn.example/f.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>G</title><guid>g</guid><link>https://show.example/?p=7</link><enclosure url="https://cdn.example/g.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>H</title><guid>h</guid><link>https://feeds.example.org/show.xml?guid=a</link><enclosure url="https://cdn.example/h.mp3" type="audio/mpeg"/></item>"#,
                r#"<item><title>I</title><guid>i</guid><link>https://show.example/other.xml?guid=1</link><enclosure url="https://cdn.example/i.mp3" type="audio/mpeg"/></item>"#,
            )
        );
        let feed: Url = "https://feeds.example.org/show.xml".parse().unwrap();
        let page = |guid: &str| {
            let mut f = fetched(&body, false);
            f.url = item_uri(&feed, guid).parse().unwrap();
            let card = crate::resolver::cards::podcast(f.url.as_str(), &f, 3600, 1000)
                .unwrap()
                .unwrap();
            assert_eq!(card.type_name, "PodcastEpisode", "{guid}");
            card.episode_page
        };
        assert_eq!(page("a").as_deref(), Some("https://show.example/a/"));
        assert_eq!(page("b"), None, "two items link it");
        assert_eq!(page("c"), None, "two items link it, one with tracking");
        assert_eq!(page("d"), None, "the show's site");
        assert_eq!(page("e"), None, "a site's root");
        assert_eq!(page("f"), None, "the feed itself");
        assert_eq!(page("g").as_deref(), Some("https://show.example/?p=7"));
        assert_eq!(page("h"), None, "another item of the same feed");
        assert_eq!(page("i"), None, "an item of another feed");
        assert!(related_hosts(
            "https://podcast.show.example/x",
            "https://show.example/y"
        ));
        assert!(related_hosts(
            "https://show.example/x",
            "https://www.show.example/y"
        ));
        assert!(!related_hosts(
            "https://show.example/x",
            "https://other.example/y"
        ));
        assert!(!related_hosts(
            "https://a.show.example/x",
            "https://b.show.example/y"
        ));
        assert!(!related_hosts(
            "https://xshow.example/x",
            "https://show.example/y"
        ));
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
