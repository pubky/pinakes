//! The title of a media page's object, for when the page hook could not name its card in time.
//! `media_records` names a TMDB, Trakt, Netflix, Goodreads, Letterboxd or IMDb page from the
//! Wikidata item that carries the page's ID, within the web fetch's few seconds; a slow Wikidata
//! leaves the page's title. This source reads the same item without that limit: the label as
//! the fact `title` and the description as `title_description`, and when the card still bears
//! another name, the resource is read again so its finished card takes them (`media::type_card`).
use super::{Context, Fact, Found, Source, Subject};
use crate::sources::media_records;
use anyhow::Result;
use async_trait::async_trait;

pub struct Titles;

#[async_trait]
impl Source for Titles {
    fn id(&self) -> &'static str {
        "titles"
    }
    fn applies(&self, subject: &Subject) -> bool {
        subject.status == "ok" && media_records::wikidata_page(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        // The fallback of `media_records`: off with it.
        if !cx.config.sources.enabled("media_records") {
            return Ok(Found::default());
        }
        let Some((property, value, _)) = media_records::wikidata_page(&subject.uri) else {
            return Ok(Found::default());
        };
        let Some((_, label, description)) =
            media_records::item(&cx.egress, property, &value).await?
        else {
            return Ok(Found::default());
        };
        let named = subject
            .doc
            .as_ref()
            .is_some_and(|d| d.source == "media_records" || d.name.as_deref() == Some(&label));
        let mut facts = vec![Fact::new("title", label.as_str())];
        facts.extend(description.map(|d| Fact::new("title_description", d)));
        Ok(Found {
            facts,
            reread: !named,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hubs::wikidata, model::ResourceMeta, sources::fixture};

    fn subject(uri: &str, name: &str, source: &str) -> Subject {
        let mut doc = ResourceMeta::new(uri, source, 3600);
        doc.name = Some(name.into());
        Subject {
            id: "0".repeat(32),
            uri: uri.into(),
            status: "ok".into(),
            type_name: Some("TVSeries".into()),
            doc: Some(doc),
            keys: vec![],
            first_seen_at: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_the_hook_left_unnamed_gets_its_title_and_is_read_again() {
        let read = |f: &str| fixture::read("media_records", f);
        let (egress, _) = fixture::egress(vec![
            (
                &wikidata::search_lookup("P4983", "1396").unwrap(),
                200,
                "application/json",
                read("search-P4983-1396.json"),
            ),
            (
                &wikidata::entities_lookup(&["Q1079".into()]),
                200,
                "application/json",
                read("entities-Q1079.json.gz"),
            ),
            (
                "https://www.wikidata.org/w/api.php?action=wbgetentities&ids=Q1079&props=labels%7Cdescriptions&languages=en&languagefallback=1&format=json",
                200,
                "application/json",
                read("labels-Q1079.json"),
            ),
        ]);
        let cx = Context {
            egress,
            config: Default::default(),
        };
        let uri = "https://www.themoviedb.org/tv/1396";
        let unnamed = subject(uri, "The Movie Database", "jsonld");
        assert!(Titles.applies(&unnamed));
        let found = Titles.lookup(&cx, &unnamed).await.unwrap();
        assert_eq!(
            found.facts,
            [
                Fact::new("title", "Breaking Bad"),
                Fact::new(
                    "title_description",
                    "American crime drama television series (2008–2013)"
                ),
            ]
        );
        assert!(found.reread);
        // A card the hook named, or that bears the title, is not read again.
        let hooked = subject(uri, "Breaking Bad", "media_records");
        assert!(!Titles.lookup(&cx, &hooked).await.unwrap().reread);
        // Only Wikidata pages: Open Library and TVmaze pages are named by their own records.
        assert!(!Titles.applies(&subject(
            "https://openlibrary.org/works/OL27482W",
            "x",
            "web"
        )));
    }
}
