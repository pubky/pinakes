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
/// an hour; `None` only when Deezer says it has none: its error 800 ("no data"), or a 404. Its
/// quota (error 4, 429), its "service busy" (error 700) and an outage are retried after the wait
/// they name; any other status or error is a failure, never "none", so the run that asked is
/// retried and its stored rows stay. An error Deezer sends with 200 is asked again after a
/// minute, not kept for the hour. Never while the source is off, nor in a build without it.
pub(crate) async fn answer(cx: &Context, path: &str) -> Result<Option<Value>> {
    if !super::enabled_by_id(&cx.config, "deezer") {
        anyhow::bail!("the deezer source is off: its API is not called");
    }
    let options = crate::resolver::egress::FetchOptions::json();
    let url = format!("{DEEZER_API}/{path}");
    let fetched = cx
        .egress
        .discover(&url, &options, cx.egress.config().discovery_secs)
        .await?;
    let busy = || super::media::busy("deezer_busy", &fetched);
    match fetched.status {
        200 => {}
        404 => return Ok(None),
        429 | 500..=599 => return Err(busy()),
        status => anyhow::bail!("deezer answered {status}"),
    }
    let v: Value = serde_json::from_slice(&fetched.body)?;
    if v["error"].is_null() {
        return Ok(Some(v));
    }
    // Deezer answers errors with 200: 800 is "no data", 4 its quota, 700 "service busy". A
    // request it cannot read (500 and 501 a parameter, 600 a query) names nothing, and asking
    // again answers the same.
    let code = v["error"]["code"].as_u64();
    if matches!(code, Some(800 | 500 | 501 | 600)) {
        return Ok(None);
    }
    cx.egress.shorten_discovery(&url, &options, 60).await?;
    match code {
        Some(4 | 700) => Err(busy()),
        Some(code) => anyhow::bail!("deezer answered error {code}"),
        None => anyhow::bail!("deezer answered an error without a code"),
    }
}

/// The album a Deezer track is on. Never while the source is off.
pub(crate) async fn track_album(cx: &Context, track: &str) -> Result<Option<String>> {
    Ok(object(cx, &format!("track/{track}"))
        .await?
        .and_then(|v| v["album"]["id"].as_u64())
        .map(|id| id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{
        egress::{Dns, Egress, FetchOptions, Fetched, Transport},
        RetryLater,
    };
    use async_trait::async_trait;
    use std::{net::SocketAddr, sync::Arc};

    struct Public;
    #[async_trait]
    impl Dns for Public {
        async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::new("8.8.8.8".parse()?, port)])
        }
    }
    /// Deezer's API, answering each path with a status and a body.
    struct Api;
    const ANSWERS: &[(&str, u16, &str)] = &[
        ("/track/1", 200, r#"{"id":1,"title":"One"}"#),
        (
            "/track/2",
            200,
            r#"{"error":{"type":"DataException","message":"no data","code":800}}"#,
        ),
        ("/track/3", 404, "{}"),
        ("/track/4", 403, "{}"),
        (
            "/track/5",
            200,
            r#"{"error":{"type":"Exception","message":"Quota limit exceeded","code":4}}"#,
        ),
        (
            "/track/6",
            200,
            r#"{"error":{"type":"Exception","message":"Service busy","code":700}}"#,
        ),
        (
            "/track/7",
            200,
            r#"{"error":{"type":"OAuthException","message":"Invalid OAuth access token.","code":300}}"#,
        ),
        ("/track/8", 503, "{}"),
        (
            "/track/9",
            200,
            r#"{"error":{"type":"ParameterException","message":"Wrong parameter","code":500}}"#,
        ),
    ];
    #[async_trait]
    impl Transport for Api {
        async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> Result<Fetched> {
            self.get_with(url, a, &FetchOptions::default()).await
        }
        async fn get_with(
            &self,
            url: &url::Url,
            _: &[SocketAddr],
            _: &FetchOptions,
        ) -> Result<Fetched> {
            let (_, status, body) = ANSWERS
                .iter()
                .find(|(path, ..)| *path == url.path())
                .unwrap_or_else(|| panic!("unexpected request {url}"));
            Ok(Fetched::answer(
                url.clone(),
                *status,
                "application/json".into(),
                body.as_bytes().to_vec(),
            ))
        }
    }

    /// Deezer has none only when it says so: its "no data" error (800), a request it cannot
    /// read (500), or a 404. A refusal
    /// (403) or an error it gives no meaning to here is a failure, and its quota, its "service
    /// busy" and an outage are waits: none of them reads as "none", which would clear what the
    /// source stored for the card.
    #[tokio::test]
    async fn only_no_data_and_a_404_are_none() {
        // The API is asked only in a build that has the `deezer` source.
        if !crate::also_on::registered()
            .iter()
            .any(|s| s.id() == "deezer")
        {
            return;
        }
        let mut config = crate::config::Config::default();
        config.also_on.sources.insert(
            "deezer".into(),
            crate::also_on::SourceOptions {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let mut egress = crate::config::EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        };
        egress.host_rates.insert("api.deezer.com".into(), 1000.0);
        let cx = Context {
            egress: Egress::with_backends(egress, Arc::new(Public), Arc::new(Api)),
            config: Arc::new(config),
        };
        let one = answer(&cx, "track/1").await.unwrap().unwrap();
        assert_eq!(one["id"], 1);
        assert_eq!(answer(&cx, "track/2").await.unwrap(), None);
        assert_eq!(object(&cx, "track/2").await.unwrap(), None);
        assert_eq!(answer(&cx, "track/3").await.unwrap(), None);
        // A request Deezer cannot read names nothing, whenever it is asked.
        assert_eq!(answer(&cx, "track/9").await.unwrap(), None);
        for path in ["track/4", "track/7"] {
            let failed = match answer(&cx, path).await {
                Ok(answer) => panic!("{path} answered {answer:?}"),
                Err(error) => error,
            };
            assert!(
                failed.downcast_ref::<RetryLater>().is_none(),
                "{path}: {failed:#}"
            );
        }
        for path in ["track/5", "track/6", "track/8"] {
            let busy = match answer(&cx, path).await {
                Ok(answer) => panic!("{path} answered {answer:?}"),
                Err(error) => error,
            };
            assert_eq!(
                busy.downcast_ref::<RetryLater>().map(|r| r.class),
                Some("deezer_busy"),
                "{path}"
            );
        }
    }
}
