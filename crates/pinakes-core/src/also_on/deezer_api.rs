//! What the sources that ask Deezer's public API share (`deezer`, and `itunes_song`,
//! `youtube_match` and `editions` for a Deezer track): one answer of the API, one object and the
//! album a track is on. The API is asked only while the `deezer` source is on, whoever asks, so
//! each of those sources goes without the others.
use super::Context;
use anyhow::Result;
use serde_json::Value;

const DEEZER_API: &str = "https://api.deezer.com";
/// One Deezer API object; `None` when Deezer has no data (its error 800) or no such ID. Never
/// while the source is off, whoever asks: the egress refuses `api.deezer.com` then too
/// (`also_on::off_hosts`).
pub(crate) async fn object(cx: &Context, path: &str) -> Result<Option<Value>> {
    Ok(answer(cx, path)
        .await?
        .filter(|v| v["id"].as_u64().is_some()))
}
/// One Deezer API answer (`path` under `https://api.deezer.com/`, its query escaped), shared for
/// an hour; `None` when Deezer answers an error (800: no data) or a client error. Its quota (error
/// 4, 429) and an outage are retried. Never while the source is off, nor in a build without it.
pub(crate) async fn answer(cx: &Context, path: &str) -> Result<Option<Value>> {
    if !super::enabled_by_id(&cx.config, "deezer") {
        anyhow::bail!("the deezer source is off: its API is not called");
    }
    let options = crate::resolver::egress::FetchOptions::json();
    let fetched = cx
        .egress
        .discover(&format!("{DEEZER_API}/{path}"), &options, 3600)
        .await?;
    let busy = || super::media::busy("deezer_busy", &fetched);
    match fetched.status {
        200 => {}
        429 | 500..=599 => return Err(busy()),
        _ => return Ok(None),
    }
    let v: Value = serde_json::from_slice(&fetched.body)?;
    // Deezer answers errors with 200: 4 is its quota, 800 "no data".
    match v["error"]["code"].as_u64() {
        None if v["error"].is_null() => Ok(Some(v)),
        Some(4) => Err(busy()),
        _ => Ok(None),
    }
}

/// The album a Deezer track is on. Never while the source is off.
pub(crate) async fn track_album(cx: &Context, track: &str) -> Result<Option<String>> {
    Ok(object(cx, &format!("track/{track}"))
        .await?
        .and_then(|v| v["album"]["id"].as_u64())
        .map(|id| id.to_string()))
}
