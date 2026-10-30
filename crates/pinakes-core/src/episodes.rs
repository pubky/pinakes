//! A podcast show's latest episodes, so a client can list and play them from the show's card
//! (TASK-podcasts, item 3). They are read from the show's feed as the show's card is built (the
//! feed's own card in `resolver::cards::podcast_body`; a Podcast Index or Apple Podcasts show's
//! card fetches the feed its record names), kept in `podcast_episode` (the latest [`KEPT`],
//! replaced whole at each save of the show, so they refresh with it) and served by
//! `GET /v1/resources/{id}/episodes` (docs/api.md, Episodes).
//!
//! Why a route and a table, not a fact: a fact is one string of at most 256 bytes per name and a
//! card's document is bounded at 8 KiB; twenty episodes with their streams are neither. Why not
//! occurrences: an episode has no instant to window by, no recurrence and no document of its
//! own here; the list is the feed's head and nothing more. An entry is not a card: the episode's
//! card is its `uri` resolved (`POST /v1/resolve`), and `id` names that card when Pinakes has it.
use crate::model::Stream;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
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
    /// The item's GUID as the feed gives it (its enclosure URL when it gives none).
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
/// The episodes of the feed at `url`, read through the egress (a discovery, cached for `ttl`
/// seconds, cut at the page cap like any feed), or `None` when the feed could not be read or is
/// none: the show's stored list then stays as it is.
pub async fn fetch(
    egress: &crate::resolver::egress::Egress,
    url: &str,
    ttl: u64,
) -> Option<Vec<Episode>> {
    // As the media route reads a feed: a feed body over the page cap is cut there, not refused
    // (the egress reads an XML feed without a feed cap like a page).
    let f = match egress.discover(url, &Default::default(), ttl).await {
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
    crate::resolver::html::blocking(move || from_feed(&f))
        .await
        .ok()
        .flatten()
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
    let link = |s: &str| crate::resolver::web::origin_url(feed, s).filter(|u| u.len() <= 2048);
    let enclosure = item
        .children()
        .find(|c| c.has_tag_name("enclosure") && c.attribute("url").is_some());
    let enclosure_url = enclosure
        .and_then(|e| e.attribute("url"))
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .and_then(link);
    let page = text("link").and_then(|l| link(&l));
    let guid = text("guid")
        .filter(|g| g.len() <= 2048)
        .or_else(|| enclosure_url.clone())
        .or_else(|| page.clone())?;
    let uri = page.clone().unwrap_or_else(|| {
        let mut u = feed.clone();
        u.query_pairs_mut().append_pair("guid", &guid);
        u.to_string()
    });
    if uri.len() > 2048 {
        return None;
    }
    let stream = enclosure.and_then(|e| {
        let url = enclosure_url.as_deref()?;
        let media_type = e
            .attribute("type")
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .or_else(|| media_type_of(url))?;
        Stream::public(
            url,
            &media_type,
            duration(text("duration").as_deref()),
            false,
        )
    });
    Some(Episode {
        id: None,
        uri,
        name: text("title").map(|t| bounded(t, 512)),
        image: child("image")
            .and_then(|c| c.attribute("href"))
            .and_then(link),
        published: text("pubDate").as_deref().and_then(date),
        guid,
        stream,
    })
}
/// Seconds of an `itunes:duration`: `3557`, `59:17` or `1:02:03` (a fraction of a second is
/// dropped).
fn duration(s: Option<&str>) -> Option<u64> {
    let parts: Vec<&str> = s?.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    parts.iter().try_fold(0u64, |acc, part| {
        let whole = part.split('.').next().unwrap_or_default();
        if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || whole.len() > 9 {
            return None;
        }
        acc.checked_mul(60)?.checked_add(whole.parse().ok()?)
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
/// The media type an enclosure without one has by its path's extension.
fn media_type_of(url: &str) -> Option<String> {
    let path = Url::parse(url).ok()?.path().to_ascii_lowercase();
    let ext = path.rsplit_once('.')?.1.to_owned();
    Some(
        match ext.as_str() {
            "mp3" => "audio/mpeg",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "ogg" | "oga" => "audio/ogg",
            "opus" => "audio/opus",
            "flac" => "audio/flac",
            "wav" => "audio/wav",
            "mp4" | "m4v" => "video/mp4",
            "webm" => "video/webm",
            _ => return None,
        }
        .to_owned(),
    )
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

/// Writes a saved document's episodes: a list replaces the show's rows whole; a document that is
/// no podcast show, or a show found missing, clears them; a show's document without a list (a
/// record read without its feed) leaves them as they are.
pub(crate) async fn store(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    doc: &crate::model::ResourceMeta,
) -> Result<()> {
    let show = doc.type_path.iter().any(|t| t == "PodcastSeries")
        && doc.status != crate::model::Status::Missing;
    match &doc.episodes {
        Some(list) if show => replace(tx, id, list).await,
        None if show => Ok(()),
        _ => {
            sqlx::query("DELETE FROM podcast_episode WHERE resource_id=$1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            Ok(())
        }
    }
}
async fn replace(tx: &mut Transaction<'_, Postgres>, id: &str, list: &[Episode]) -> Result<()> {
    sqlx::query("DELETE FROM podcast_episode WHERE resource_id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    let list = &list[..list.len().min(KEPT)];
    if list.is_empty() {
        return Ok(());
    }
    let positions: Vec<i16> = (0..list.len() as i16).collect();
    let guids: Vec<&str> = list.iter().map(|e| e.guid.as_str()).collect();
    let urls: Vec<&str> = list.iter().map(|e| e.uri.as_str()).collect();
    let names: Vec<Option<&str>> = list.iter().map(|e| e.name.as_deref()).collect();
    let images: Vec<Option<&str>> = list.iter().map(|e| e.image.as_deref()).collect();
    let published: Vec<Option<DateTime<Utc>>> = list.iter().map(|e| e.published).collect();
    let streams: Vec<Option<String>> = list
        .iter()
        .map(|e| e.stream.as_ref().map(serde_json::to_string).transpose())
        .collect::<Result<_, _>>()?;
    sqlx::query("INSERT INTO podcast_episode(resource_id,position,guid,url,name,image,published,stream) SELECT $1,p,g,u,n,i,d,s::jsonb FROM unnest($2::smallint[],$3::text[],$4::text[],$5::text[],$6::text[],$7::timestamptz[],$8::text[]) AS t(p,g,u,n,i,d,s)")
        .bind(id).bind(&positions).bind(&guids).bind(&urls).bind(&names).bind(&images).bind(&published).bind(&streams)
        .execute(&mut **tx).await?;
    Ok(())
}

/// A stored episode's row: GUID, URI, name, image, date and stream (as JSON text).
type Row = (
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
#[utoipa::path(get,path="/v1/resources/{id}/episodes",operation_id="list_episodes",tag="Media",params(("id"=String,Path),EpisodeQuery),responses((status=200,body=EpisodePage),(status=400,description="Invalid resource ID or limit"),(status=404,description="Resource not found")),description="A podcast show's latest episodes as its feed lists them, newest first: at most 20, each with its page or feed item, date and public stream. The show's own list, else that of a member of its group which has one (an Apple Podcasts show grouped with its feed). A known resource that is no show, or whose feed was not read yet, returns an empty list.")]
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
    let root: Option<String> = sqlx::query_scalar("SELECT r.id FROM resource r WHERE r.id=coalesce((SELECT root_id FROM resource_alias WHERE alias_id=$1),$1)")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(root) = root else {
        return Err(ApiError(
            axum::http::StatusCode::NOT_FOUND,
            "resource not found".into(),
        ));
    };
    let rows: Vec<Row> = sqlx::query_as(
        "WITH own AS (SELECT $1::text AS id WHERE EXISTS (SELECT 1 FROM podcast_episode WHERE resource_id=$1)),\
         member AS (SELECT m.resource_id AS id FROM group_unit u JOIN group_unit m ON m.group_id=u.group_id WHERE u.unit_id=$1 AND u.group_id IS NOT NULL AND m.resource_id<>$1 AND m.occurrence_key='' AND EXISTS (SELECT 1 FROM podcast_episode e WHERE e.resource_id=m.resource_id) ORDER BY (SELECT max(published) FROM podcast_episode e WHERE e.resource_id=m.resource_id) DESC NULLS LAST,m.resource_id LIMIT 1),\
         chosen AS (SELECT id FROM own UNION ALL SELECT id FROM member WHERE NOT EXISTS (SELECT 1 FROM own))\
         SELECT e.guid,e.url,e.name,e.image,e.published,e.stream::text FROM podcast_episode e JOIN chosen c ON c.id=e.resource_id ORDER BY e.position LIMIT $2",
    )
    .bind(&root)
    .bind(limit as i64)
    .fetch_all(&state.pool)
    .await?;
    let mut items: Vec<Episode> = rows
        .into_iter()
        .map(|(guid, uri, name, image, published, stream)| Episode {
            id: None,
            uri,
            name,
            image,
            published,
            guid,
            stream: stream.and_then(|s| serde_json::from_str(&s).ok()),
        })
        .collect();
    // The episodes Pinakes has as resources, by their page's identity (an alias as its root).
    let identities: Vec<String> = items
        .iter()
        .filter_map(|e| crate::model::identity(&e.uri).ok().map(|v| v.0))
        .collect();
    let known: Vec<(String, String)> = sqlx::query_as("SELECT i,r.id FROM unnest($1::text[]) i LEFT JOIN resource_alias a ON a.alias_id=i JOIN resource r ON r.id=coalesce(a.root_id,i)")
        .bind(&identities)
        .fetch_all(&state.pool)
        .await?;
    for e in &mut items {
        let identity = crate::model::identity(&e.uri).ok().map(|v| v.0);
        e.id = known
            .iter()
            .find(|(i, _)| Some(i) == identity.as_ref())
            .map(|(_, id)| id.clone());
    }
    Ok(axum::Json(EpisodePage { items }))
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

    #[test]
    fn a_feeds_items_are_its_latest_episodes_newest_first_with_their_public_streams() {
        let body = feed(concat!(
            // Out of order: the second item is the newest.
            r#"<item><title>Two</title><guid isPermaLink="false">ep-2</guid><link>https://show.example/2</link><pubDate>Tue, 20 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>59:17</itunes:duration><itunes:image href="/art/2.jpg"/><enclosure url="https://cdn.example/2.mp3" type="audio/mpeg" length="100"/></item>"#,
            r#"<item><title>Three</title><guid>ep-3</guid><link>https://show.example/3</link><pubDate>Wed, 21 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>1:02:03</itunes:duration><enclosure url="https://cdn.example/3.mp3"/></item>"#,
            // A signed enclosure is listed without a stream.
            r#"<item><title>One</title><guid>ep-1</guid><link>https://show.example/1</link><pubDate>Mon, 19 Oct 2026 10:00:00 +0000</pubDate><itunes:duration>3557</itunes:duration><enclosure url="https://cdn.example/1.mp3?Expires=1&amp;Signature=abc" type="audio/mpeg"/></item>"#,
            // No GUID and no link: the enclosure names it, and its feed item is its URI.
            r#"<item><title>Zero</title><enclosure url="https://cdn.example/0.m4a" type="audio/mp4"/></item>"#,
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
            }),
            "a type the enclosure lacks comes from its extension"
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
        assert_eq!(zero.guid, "https://cdn.example/0.m4a");
        assert_eq!(
            zero.uri,
            "https://feeds.example.org/show.xml?guid=https%3A%2F%2Fcdn.example%2F0.m4a"
        );
        assert_eq!(zero.published, None);
        assert_eq!(
            zero.stream
                .as_ref()
                .map(|s| (s.media_type.as_str(), s.duration_s)),
            Some(("audio/mp4", None))
        );
        // A feed cut at the cap inside its last item reads the whole ones.
        let cut = &body[..body.find("<item><title>Note").unwrap() + 20];
        assert_eq!(from_feed(&fetched(cut, true)).unwrap().len(), 4);
        assert_eq!(from_feed(&fetched("<html></html>", false)), None);
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
    fn durations_and_dates_as_feeds_write_them() {
        assert_eq!(duration(Some("3557")), Some(3557));
        assert_eq!(duration(Some("59:17")), Some(3557));
        assert_eq!(duration(Some("1:02:03")), Some(3723));
        assert_eq!(duration(Some("01:02:03.500")), Some(3723));
        assert_eq!(duration(Some("")), None);
        assert_eq!(duration(Some("1:2:3:4")), None);
        assert_eq!(duration(Some("an hour")), None);
        assert_eq!(duration(None), None);
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
