//! The `wikipedia` also-on source.
use crate::{
    also_on::{Context, Found, Source, Subject},
    hubs::wikidata,
};
use anyhow::Result;
use async_trait::async_trait;

use crate::wikipedia::*;

/// Language editions and other encyclopedias of a Wikipedia article.
pub struct WikipediaSource;
#[async_trait]
impl Source for WikipediaSource {
    fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
        &[("wikidata", "Q42")]
    }
    fn id(&self) -> &'static str {
        "wikipedia"
    }
    fn applies(&self, subject: &Subject) -> bool {
        parse(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        crate::also_on::unless_over_cap(self.read(cx, subject).await, Found::default)
    }
}
impl WikipediaSource {
    async fn read(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(article) = parse(&subject.uri) else {
            return Ok(Found::default());
        };
        let url = item_lookup(&article)?;
        let options = crate::resolver::egress::FetchOptions::json();
        let fetched = cx
            .egress
            .discover(url.as_str(), &options, cx.egress.config().discovery_secs)
            .await?;
        if matches!(fetched.status, 429 | 500..=599) {
            return Err(super::media::busy("wikipedia_busy", &fetched));
        }
        // Only an article its Wikipedia does not have, or one without an item, is "nothing".
        let Some(qid) = item_answer(fetched.status, &fetched.body)? else {
            return Ok(Found::default());
        };
        // The item over the cap: the article keeps the key its own Wikipedia proved.
        let read = wikidata::items(&cx.egress, std::slice::from_ref(&qid)).await;
        crate::also_on::unless_over_cap(
            read.map(|items| {
                items
                    .first()
                    .map(|item| found(item, &article))
                    .unwrap_or_default()
            }),
            || crate::also_on::wikidata_key(&qid),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookups::fixture::{at, context, JSON};

    /// A Wikipedia that refuses or is busy is an error, so the run comes back and the article's
    /// stored rows stay; only an article its Wikipedia does not have is "nothing".
    #[tokio::test(start_paused = true)]
    async fn a_failed_item_read_is_an_error_and_a_missing_article_is_nothing() {
        let article = at("https://en.wikipedia.org/wiki/Paris");
        let lookup = item_lookup(&parse(&article.uri).unwrap()).unwrap();
        for (status, body) in [
            (403, &b"{}"[..]),
            (503, &b"{}"[..]),
            (
                200,
                &br#"{"error":{"code":"ratelimited","info":"slow down"}}"#[..],
            ),
        ] {
            let cx = context(vec![(lookup.as_str(), status, JSON, body.to_vec())]);
            assert!(
                WikipediaSource.lookup(&cx, &article).await.is_err(),
                "{status}"
            );
        }
        for (status, body) in [
            (404, &b"{}"[..]),
            (
                200,
                &br#"{"batchcomplete":"","query":{"pages":{"-1":{"ns":0,"title":"Paris","missing":""}}}}"#[..],
            ),
        ] {
            let cx = context(vec![(lookup.as_str(), status, JSON, body.to_vec())]);
            assert_eq!(
                WikipediaSource.lookup(&cx, &article).await.unwrap(),
                Found::default(),
                "{status}"
            );
        }
    }
}
