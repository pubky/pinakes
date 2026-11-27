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
/// `None` only for a 404 or a 400 (an ID iTunes cannot read: asking again answers the same). A
/// rate limit or an outage is retried after the wait it names; any other status is a failure,
/// never "none", so the run that asked is retried and its stored rows stay.
pub(crate) async fn answer(cx: &Context, url: &str) -> Result<Option<Value>> {
    let fetched = cx
        .egress
        .discover(url, &options(), cx.egress.config().discovery_secs)
        .await?;
    crate::resolver::busy(&fetched, "itunes_busy")?;
    match fetched.status {
        200 => {}
        400 | 404 => return Ok(None),
        status => anyhow::bail!("itunes answered {status}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::{
        egress::{Dns, Egress, Fetched, Transport},
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
    /// iTunes' lookup, answering with the status its `id` names (a result list for 200).
    struct Lookup;
    #[async_trait]
    impl Transport for Lookup {
        async fn get(&self, url: &url::Url, a: &[SocketAddr]) -> Result<Fetched> {
            self.get_with(url, a, &FetchOptions::default()).await
        }
        async fn get_with(
            &self,
            url: &url::Url,
            _: &[SocketAddr],
            _: &FetchOptions,
        ) -> Result<Fetched> {
            let id = url
                .query_pairs()
                .find(|(key, _)| key == "id")
                .map(|(_, id)| id.into_owned())
                .unwrap_or_default();
            let status: u16 = id.parse()?;
            let body = if status == 200 {
                "\n\n\n{\"resultCount\":0,\"results\":[]}"
            } else {
                "{}"
            };
            Ok(Fetched::answer(
                url.clone(),
                status,
                "text/javascript".into(),
                body.as_bytes().to_vec(),
            ))
        }
    }

    /// iTunes has none only when it answers a result list without the thing, or a 404. A
    /// refusal (403) or another status it gives no meaning to here is a failure, and an outage
    /// is a wait: neither reads as "none", which would clear what the source stored for the
    /// card.
    #[tokio::test]
    async fn only_a_404_is_none() {
        let mut egress = crate::config::EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        };
        egress.host_rates.insert("itunes.apple.com".into(), 1000.0);
        let cx = Context {
            egress: Egress::with_backends(egress, Arc::new(Public), Arc::new(Lookup)),
            config: Arc::new(crate::config::Config::default()),
        };
        let found = lookup(&cx, "200").await.unwrap().unwrap();
        assert_eq!(found["resultCount"], 0);
        assert_eq!(lookup(&cx, "404").await.unwrap(), None);
        // An ID iTunes cannot read (400) is no object either: it answers the same every time.
        assert_eq!(lookup(&cx, "400").await.unwrap(), None);
        for status in ["403", "401"] {
            let failed = match lookup(&cx, status).await {
                Ok(answer) => panic!("{status} answered {answer:?}"),
                Err(error) => error,
            };
            assert!(
                failed.downcast_ref::<RetryLater>().is_none(),
                "{status}: {failed:#}"
            );
        }
        // A timeout of the request (408) is a wait, as an outage is (`resolver::busy`).
        for status in ["503", "408"] {
            let busy = lookup(&cx, status).await.unwrap_err();
            assert_eq!(
                busy.downcast_ref::<RetryLater>().map(|r| r.class),
                Some("itunes_busy"),
                "{status}"
            );
        }
    }
}
