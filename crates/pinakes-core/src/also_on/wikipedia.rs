//! The `wikipedia` also-on source.
use crate::{
    also_on::{Context, Found, Source, Subject},
    hubs::wikidata,
};
use anyhow::Result;
use async_trait::async_trait;
use url::Url;

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
        let mut url = Url::parse(&format!("https://{}.wikipedia.org/w/api.php", article.lang))?;
        url.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("prop", "pageprops")
            .append_pair("ppprop", "wikibase_item")
            .append_pair("titles", &article.title)
            .append_pair("redirects", "1")
            .append_pair("format", "json");
        let options = crate::resolver::egress::FetchOptions::json();
        let fetched = cx.egress.discover(url.as_str(), &options, 3600).await?;
        match fetched.status {
            200 => {}
            429 | 500..=599 => return Err(super::media::busy("wikipedia_busy", &fetched)),
            _ => return Ok(Found::default()),
        }
        let Some(qid) = parse_item(&serde_json::from_slice(&fetched.body)?) else {
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
