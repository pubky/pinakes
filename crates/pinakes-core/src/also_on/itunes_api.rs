//! What the sources that ask iTunes' Search and Lookup API share (`itunes`, `itunes_song`,
//! `editions` for an Apple Music track, and `books` and `podcasts` for how it is asked): the
//! request's options, one answer, a track's lookup and the album a track is on. Each of those
//! sources goes without the others.
use super::Context;
use crate::resolver::egress::FetchOptions;
use anyhow::Result;
use serde_json::Value;

/// How iTunes is asked: for JSON, which it serves as `text/javascript`.
pub(crate) fn options() -> FetchOptions {
    FetchOptions {
        extra_types: &["text/javascript"],
        ..FetchOptions::json()
    }
}
/// One iTunes Search or Lookup answer (`url` in the configured storefront), shared for an hour;
/// `None` for an answer that is no result list (4xx). A rate limit or an outage is retried.
pub(crate) async fn answer(cx: &Context, url: &str) -> Result<Option<Value>> {
    let fetched = cx.egress.discover(url, &options(), 3600).await?;
    match fetched.status {
        200 => {}
        429 | 500..=599 => return Err(super::media::busy("itunes_busy", &fetched)),
        _ => return Ok(None),
    }
    // iTunes answers `text/javascript` with leading newlines.
    Ok(Some(serde_json::from_slice(fetched.body.trim_ascii())?))
}
/// iTunes' `lookup?id={id}&entity=song` in the configured storefront.
pub(crate) async fn lookup(cx: &Context, id: &str) -> Result<Option<Value>> {
    let storefront = &cx.config.also_on.apple_storefront;
    let url = format!("https://itunes.apple.com/lookup?id={id}&entity=song&country={storefront}");
    answer(cx, &url).await
}
/// The album an Apple Music track is on (the same lookup as its preview's).
pub(crate) async fn track_album(cx: &Context, track: &str) -> Result<Option<String>> {
    Ok(lookup(cx, track).await?.and_then(|v| {
        v["results"].as_array()?.iter().find_map(|r| {
            (r["wrapperType"] == "track"
                && r["trackId"].as_u64().map(|id| id.to_string()).as_deref() == Some(track))
            .then(|| r["collectionId"].as_u64())
            .flatten()
            .map(|id| id.to_string())
        })
    }))
}
