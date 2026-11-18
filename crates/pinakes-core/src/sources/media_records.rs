//! Media pages named after the object they show. A film's, series', book's or podcast's page on a
//! service often names the site, another language's title, or nothing at all (an app shell): the
//! card then takes the object's name, and where the page's own text is not the object's, its
//! description, from the object's exact record. A record is exact: the Wikidata item that carries
//! the page's own ID on that service, the service's own record by that ID, never a search by
//! name. A page hook (and, for walled pages, a wall hook): an error leaves the page's own card.
use super::{Context, Example, Source};
use crate::{
    also_on::media::{exact_page, imdb, title_type},
    hubs::wikidata,
    model::{ResourceMeta, Status},
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct MediaRecords;

/// What a page's own text is worth once the record names its object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text {
    /// The page describes its object: keep its description and image.
    Own,
    /// The page's description is not the object's (another language, an edition list): the
    /// record's description replaces it; its image stays.
    Record,
    /// The page is the service's app shell: the record's description replaces the page's, and the
    /// service's own image goes.
    Shell,
}

/// The record that names a page's object.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    /// The Wikidata item carrying the page's ID (`property` = `value`).
    Wikidata {
        property: &'static str,
        value: String,
        text: Text,
    },
    /// Open Library's own record: `works/OL…W` or `books/OL…M`.
    OpenLibrary(String),
    /// TVmaze's record of a show.
    Tvmaze(u64),
    /// A Podcast Index page: its title is the feed's, followed by the site's name.
    PodcastIndex,
}

/// The record that names the object of a page, by its link alone (the object's own page only:
/// never a cast, review or season page under it).
fn record(uri: &str) -> Option<Record> {
    let url = super::kit::web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let wikidata = |property, value: &str, text| Record::Wikidata {
        property,
        value: value.to_owned(),
        text,
    };
    let digits = |v: &str| !v.is_empty() && v.len() <= 12 && v.bytes().all(|b| b.is_ascii_digit());
    let slug = |v: &str| {
        !v.is_empty()
            && v.len() <= 200
            && v.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    Some(match (host, s.as_slice()) {
        ("themoviedb.org", [kind @ ("movie" | "tv"), id]) => {
            let id = id.split('-').next()?;
            if !digits(id) {
                return None;
            }
            let property = if *kind == "movie" { "P4947" } else { "P4983" };
            wikidata(property, id, Text::Record)
        }
        ("trakt.tv", [kind @ ("movies" | "shows"), name]) if slug(name) => {
            wikidata("P8013", &format!("{kind}/{name}"), Text::Shell)
        }
        ("netflix.com", ["title", id] | [_, "title", id]) if digits(id) => {
            wikidata("P1874", id, Text::Shell)
        }
        ("goodreads.com", ["work", "editions", id]) => {
            let id = id.split(['-', '.']).next()?;
            if !digits(id) {
                return None;
            }
            wikidata("P8383", id, Text::Record)
        }
        ("letterboxd.com", ["film", name]) if slug(name) => wikidata("P6127", name, Text::Own),
        // IMDb answers bots with an empty page (or a challenge: the wall hook).
        ("imdb.com" | "m.imdb.com", ["title", tt]) if imdb(tt) => {
            wikidata("P345", tt, Text::Record)
        }
        ("openlibrary.org", [kind @ ("works" | "books"), id, ..]) => {
            let suffix = if *kind == "works" { 'W' } else { 'M' };
            let ok = id.len() <= 16
                && id
                    .strip_prefix("OL")
                    .and_then(|r| r.strip_suffix(suffix))
                    .is_some_and(digits);
            if !ok || s.len() > 3 {
                return None;
            }
            Record::OpenLibrary(format!("{kind}/{id}"))
        }
        // The show's own page (`/shows/{id}`, `/shows/{id}/{slug}`), never its episodes, cast or
        // other sub-pages.
        ("tvmaze.com", ["shows", id, ..]) if digits(id) && s.len() <= 3 && exact_page(uri) => {
            Record::Tvmaze(id.parse().ok()?)
        }
        ("podcastindex.org", ["podcast", id]) if digits(id) && url.query().is_none() => {
            Record::PodcastIndex
        }
        _ => return None,
    })
}

/// A walled page's object, by its link alone: an IMDb title (by its Wikidata item), or a player's
/// page of an Apple podcast (by iTunes).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Walled {
    Imdb(String),
    ApplePodcast(u64),
}
fn walled(uri: &str) -> Option<Walled> {
    let url = super::kit::web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (host.as_str(), s.as_slice()) {
        ("www.imdb.com" | "imdb.com" | "m.imdb.com", ["title", tt]) if imdb(tt) => {
            Some(Walled::Imdb((*tt).to_owned()))
        }
        ("pca.st" | "castro.fm", ["itunes", id]) => id.parse().ok().map(Walled::ApplePodcast),
        // Overcast's `itunes{id}`, with or without the show's slug, signs a reader in first.
        ("overcast.fm" | "www.overcast.fm", [id] | [id, _]) => id
            .strip_prefix("itunes")
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|id| id.parse().ok())
            .map(Walled::ApplePodcast),
        _ => None,
    }
}

/// `wbgetentities` for an item's English label and description.
fn labels_lookup(qid: &str) -> String {
    format!("{}?action=wbgetentities&ids={qid}&props=labels%7Cdescriptions&languages=en&languagefallback=1&format=json", wikidata::API)
}
/// An item's English label and description.
fn parse_labels(raw: &[u8], qid: &str) -> Option<(String, Option<String>)> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let entity = &v["entities"][qid];
    let text = |key: &str| super::text(&entity[key]["en"]["value"]);
    Some((text("labels")?, text("descriptions")))
}

/// The Wikidata ID a page's object is found by, and what the page's own text is worth: for the
/// pages whose record is a Wikidata item (the also-on `titles` source reads the same).
pub fn wikidata_page(uri: &str) -> Option<(&'static str, String, Text)> {
    match record(uri) {
        Some(Record::Wikidata {
            property,
            value,
            text,
        }) => Some((property, value, text)),
        _ => None,
    }
}

/// The item carrying `property` = `value`, and its English label and description.
pub async fn item(
    egress: &crate::resolver::egress::Egress,
    property: &str,
    value: &str,
) -> Result<Option<(wikidata::Item, String, Option<String>)>> {
    let wikidata::Found::One(item) = wikidata::item_with(egress, property, value).await? else {
        return Ok(None);
    };
    let f = egress
        .discover(&labels_lookup(&item.qid), &FetchOptions::json(), 86400)
        .await?;
    // A busy Wikidata (an HTTP error, or an API error sent with 200) is asked again later, never
    // taken for "no label".
    super::retry(&f)?;
    if f.status != 200 {
        return Ok(None);
    }
    let error = serde_json::from_slice::<Value>(&f.body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_owned));
    if let Some(code) = error {
        egress
            .shorten_discovery(&labels_lookup(&item.qid), &FetchOptions::json(), 1)
            .await?;
        return Err(crate::resolver::RetryLater {
            class: "wikidata_busy",
            seconds: f.retry_after_secs.clamp(60, 3600),
        })
        .with_context(|| format!("Wikidata labels: {code}"));
    }
    Ok(parse_labels(&f.body, &item.qid).map(|(label, description)| (*item, label, description)))
}

/// A card's name (and, per `text`, its description and image) from its item's label.
fn titled(doc: &mut ResourceMeta, label: String, description: Option<String>, text: Text) {
    doc.name = Some(label);
    if text != Text::Own {
        doc.description = description;
    }
    if text == Text::Shell {
        doc.image = None;
    }
}
/// A finished card of a Wikidata page that the hook did not name (its request timed out, say)
/// named from the title the also-on `titles` source found since. Whether it applied.
fn apply_title(uri: &str, title: &str, description: Option<&str>, doc: &mut ResourceMeta) -> bool {
    let Some((_, _, text)) = wikidata_page(uri) else {
        return false;
    };
    if doc.source == "media_records" || doc.name.as_deref() == Some(title) {
        return false;
    }
    titled(doc, title.to_owned(), description.map(str::to_owned), text);
    true
}

/// A page's card named by its Wikidata item: the label, the item as a fact, and for a film or
/// series its type.
fn named_by_item(
    doc: &ResourceMeta,
    item: &wikidata::Item,
    label: String,
    description: Option<String>,
    text: Text,
) -> ResourceMeta {
    let mut doc = doc.clone();
    titled(&mut doc, label, description, text);
    if let Some(kind) = title_type(item) {
        doc.set_type(kind);
    }
    doc.attributes.insert("wikidata".into(), item.qid.clone());
    doc.source = "media_records".into();
    doc
}

/// Text from a record that may be a string or `{"value": …}` (Open Library's descriptions).
fn text_value(v: &Value) -> Option<String> {
    v.as_str()
        .or_else(|| v["value"].as_str())
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
}
fn excerpt(s: String) -> String {
    s.chars().take(300).collect()
}

/// A page's card named by Open Library's record of its work or edition (`path`).
fn named_by_open_library(doc: &ResourceMeta, path: &str, raw: &[u8]) -> Option<ResourceMeta> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let kind = if path.starts_with("works/") {
        "/type/work"
    } else {
        "/type/edition"
    };
    // A merged record is a redirect, and names another object.
    if v["type"]["key"] != kind || v["key"].as_str() != Some(&format!("/{path}")) {
        return None;
    }
    let title = text_value(&v["title"])?;
    let mut doc = doc.clone();
    doc.name = Some(title);
    // Open Library's descriptions may carry HTML.
    if let Some(description) = text_value(&v["description"]).and_then(|d| super::kit::html_text(&d))
    {
        doc.description = Some(excerpt(description));
    }
    doc.source = "media_records".into();
    Some(doc)
}

/// A page's card named by TVmaze's record of show `id`.
fn named_by_tvmaze(doc: &ResourceMeta, id: u64, raw: &[u8]) -> Option<ResourceMeta> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    if v["id"].as_u64() != Some(id) {
        return None;
    }
    let mut doc = doc.clone();
    doc.name = Some(text_value(&v["name"])?);
    if let Some(summary) = v["summary"].as_str().and_then(super::kit::html_text) {
        doc.description = Some(excerpt(summary));
    }
    if let Some(image) = v["image"]["original"]
        .as_str()
        .filter(|u| u.starts_with("https://"))
    {
        doc.image = Some(image.to_owned());
    }
    doc.set_type("TVSeries");
    doc.source = "media_records".into();
    Some(doc)
}

/// A Podcast Index page's card without the site's name in its title.
fn named_by_podcast_index(doc: &ResourceMeta) -> Option<ResourceMeta> {
    let name = doc
        .name
        .as_deref()?
        .strip_suffix(" | Podcastindex.org")?
        .trim();
    if name.is_empty() {
        return None;
    }
    let mut doc = doc.clone();
    doc.name = Some(name.to_owned());
    Some(doc)
}

/// The feed iTunes' record of a show names (`feedUrl`), when it is an http(s) URL.
fn itunes_feed(raw: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let [one] = v["results"].as_array()?.as_slice() else {
        return None;
    };
    one["feedUrl"]
        .as_str()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(str::to_owned)
}
/// The card of an Apple podcast's player page from iTunes' record of the show.
fn itunes_card(cx: &Context<'_>, uri: &str, show: u64, raw: &[u8]) -> Option<ResourceMeta> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let [one] = v["results"].as_array()?.as_slice() else {
        return None;
    };
    if one["kind"] != "podcast" || one["collectionId"].as_u64() != Some(show) {
        return None;
    }
    let mut doc = cx.card(uri, &MediaRecords);
    doc.set_type("PodcastSeries");
    doc.name = Some(text_value(&one["collectionName"])?);
    doc.image = one["artworkUrl600"]
        .as_str()
        .filter(|u| u.starts_with("https://"))
        .map(str::to_owned);
    doc.status = Status::Ok;
    Some(doc)
}

#[async_trait]
impl Source for MediaRecords {
    fn name(&self) -> &'static str {
        "media_records"
    }
    fn apply_title(
        &self,
        uri: &str,
        title: &str,
        description: Option<&str>,
        doc: &mut ResourceMeta,
    ) -> bool {
        apply_title(uri, title, description, doc)
    }
    fn reads(&self, _uri: &str) -> bool {
        false
    }
    fn hooks_pages(&self) -> bool {
        true
    }
    fn hooks_walls(&self) -> bool {
        true
    }
    fn facts(&self) -> &'static [&'static str] {
        &["wikidata"]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://www.themoviedb.org/movie/603",
                card_type: "Movie",
            },
            Example {
                uri: "https://www.themoviedb.org/tv/1396",
                card_type: "TVSeries",
            },
            Example {
                uri: "https://trakt.tv/shows/breaking-bad",
                card_type: "TVSeries",
            },
            Example {
                uri: "https://www.netflix.com/title/70143836",
                card_type: "TVSeries",
            },
            Example {
                uri: "https://openlibrary.org/works/OL27482W",
                card_type: "Book",
            },
            Example {
                uri: "https://www.goodreads.com/work/editions/1540236",
                card_type: "Book",
            },
            Example {
                uri: "https://www.tvmaze.com/shows/169/breaking-bad",
                card_type: "TVSeries",
            },
            Example {
                uri: "https://podcastindex.org/podcast/577105",
                card_type: "PodcastSeries",
            },
        ]
    }
    async fn resolve(&self, _cx: &Context<'_>, _uri: &str) -> Result<ResourceMeta> {
        anyhow::bail!("media pages are read by the web route")
    }
    async fn page(
        &self,
        cx: &Context<'_>,
        uri: &str,
        _page: &Fetched,
        doc: &ResourceMeta,
    ) -> Result<Option<ResourceMeta>> {
        let Some(record) = record(uri) else {
            return Ok(None);
        };
        Ok(match record {
            Record::Wikidata {
                property,
                value,
                text,
            } => item(cx.egress, property, &value)
                .await?
                .map(|(item, label, description)| {
                    named_by_item(doc, &item, label, description, text)
                }),
            Record::OpenLibrary(path) => {
                let f = cx
                    .egress
                    .discover(
                        &format!("https://openlibrary.org/{path}.json"),
                        &FetchOptions::json(),
                        3600,
                    )
                    .await?;
                super::retry(&f)?;
                if f.status != 200 {
                    return Ok(None);
                }
                let doc = doc.clone();
                crate::resolver::html::blocking(move || named_by_open_library(&doc, &path, &f.body))
                    .await?
            }
            Record::Tvmaze(id) => {
                let f = cx
                    .egress
                    .discover(
                        &format!("https://api.tvmaze.com/shows/{id}"),
                        &FetchOptions::json(),
                        3600,
                    )
                    .await?;
                super::retry(&f)?;
                if f.status != 200 {
                    return Ok(None);
                }
                let doc = doc.clone();
                crate::resolver::html::blocking(move || named_by_tvmaze(&doc, id, &f.body)).await?
            }
            Record::PodcastIndex => named_by_podcast_index(doc),
        })
    }
    async fn walled(&self, cx: &Context<'_>, uri: &str) -> Result<Option<ResourceMeta>> {
        Ok(match walled(uri) {
            Some(Walled::Imdb(tt)) => {
                item(cx.egress, "P345", &tt)
                    .await?
                    .map(|(item, label, description)| {
                        let mut doc = cx.card(uri, self);
                        doc.set_type("CreativeWork");
                        doc.status = Status::Ok;
                        named_by_item(&doc, &item, label, description, Text::Record)
                    })
            }
            Some(Walled::ApplePodcast(show)) => {
                let options = FetchOptions {
                    extra_types: &["text/javascript"],
                    ..FetchOptions::json()
                };
                let f = cx
                    .egress
                    .discover(
                        // The lookup the podcasts source asks too (the operator's storefront is not
                        // in a hook's context; a show's name is the same in every storefront).
                        &format!(
                            "https://itunes.apple.com/lookup?id={show}&entity=podcast&country=us"
                        ),
                        &options,
                        3600,
                    )
                    .await?;
                super::retry(&f)?;
                match (f.status == 200)
                    .then(|| itunes_card(cx, uri, show, &f.body))
                    .flatten()
                {
                    Some(mut doc) => {
                        // The show's latest episodes, from the feed iTunes names for it
                        // (`episodes`), within what is left of the read's deadline after the
                        // lookup, less what hands the card back: a slow feed, or a wait for a
                        // parser, costs the list, never the card.
                        if let Some(feed) = itunes_feed(&f.body) {
                            if let Some(listed) = crate::episodes::fetch(
                                cx.egress,
                                &feed,
                                3600,
                                crate::episodes::feed_deadline(cx.deadline()),
                            )
                            .await
                            {
                                doc.episodes = Some(listed.episodes);
                                doc.episodes_older = listed.older;
                                doc.episodes_cut = listed.cut;
                            }
                        }
                        Some(doc)
                    }
                    None => None,
                }
            }
            None => None,
        })
    }
}

pub mod titles {
    //! The also-on half of this source, `titles`: the title of a media page's object, for when the
    //! page hook could not name its card in time.
    //! `media_records` names a TMDB, Trakt, Netflix, Goodreads, Letterboxd or IMDb page from the
    //! Wikidata item that carries the page's ID, within the web fetch's few seconds; a slow Wikidata
    //! leaves the page's title. This source reads the same item without that limit: the label as
    //! the fact `title` and the description as `title_description`, and when the card still bears
    //! another name, the resource is read again so its finished card takes them (`media::type_card`).
    use crate::also_on::{Context, Fact, Found, Source, Subject};
    use anyhow::Result;
    use async_trait::async_trait;

    pub struct Titles;

    #[async_trait]
    impl Source for Titles {
        fn id(&self) -> &'static str {
            "titles"
        }
        fn page(&self) -> &'static str {
            "media_records"
        }
        fn applies(&self, subject: &Subject) -> bool {
            subject.status == "ok" && super::wikidata_page(&subject.uri).is_some()
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
            // The fallback of `media_records`: off with it.
            if !cx.config.sources.enabled("media_records") {
                return Ok(Found::default());
            }
            let Some((property, value, _)) = super::wikidata_page(&subject.uri) else {
                return Ok(Found::default());
            };
            let Some((_, label, description)) = super::item(&cx.egress, property, &value).await?
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
                candidates: vec![],
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    fn read(file: &str) -> Vec<u8> {
        fixture::read("media_records", file)
    }
    fn web_card(uri: &str, name: &str) -> ResourceMeta {
        let mut doc = ResourceMeta::new(uri, "jsonld", 3600);
        doc.set_type("WebPage");
        doc.name = Some(name.into());
        doc.description = Some("Parhaan miespuolisen draamanäyttelijän Emmyn voittanut".into());
        doc.image = Some("https://image.tmdb.org/t/p/w500/poster.jpg".into());
        doc
    }

    #[test]
    fn the_object_s_own_page_names_its_record() {
        let wd = |property, value: &str, text| {
            Some(Record::Wikidata {
                property,
                value: value.into(),
                text,
            })
        };
        for (uri, want) in [
            (
                "https://www.themoviedb.org/movie/603",
                wd("P4947", "603", Text::Record),
            ),
            (
                "https://www.themoviedb.org/tv/1396-breaking-bad",
                wd("P4983", "1396", Text::Record),
            ),
            (
                "https://trakt.tv/shows/breaking-bad",
                wd("P8013", "shows/breaking-bad", Text::Shell),
            ),
            (
                "https://www.netflix.com/gb/title/70143836",
                wd("P1874", "70143836", Text::Shell),
            ),
            (
                "https://www.goodreads.com/work/editions/1540236-the-hobbit",
                wd("P8383", "1540236", Text::Record),
            ),
            (
                "https://letterboxd.com/film/the-matrix/",
                wd("P6127", "the-matrix", Text::Own),
            ),
            (
                "https://openlibrary.org/works/OL27482W/The_Hobbit",
                Some(Record::OpenLibrary("works/OL27482W".into())),
            ),
            (
                "https://openlibrary.org/books/OL23109860M",
                Some(Record::OpenLibrary("books/OL23109860M".into())),
            ),
            (
                "https://www.tvmaze.com/shows/169/breaking-bad",
                Some(Record::Tvmaze(169)),
            ),
            (
                "https://podcastindex.org/podcast/577105",
                Some(Record::PodcastIndex),
            ),
            // Pages under the object, and other objects, keep their own cards.
            ("https://www.themoviedb.org/movie/603/cast", None),
            ("https://trakt.tv/shows/breaking-bad/seasons/1", None),
            ("https://podcastindex.org/podcast/577105?episode=1", None),
            (
                "https://openlibrary.org/works/OL27482W/The_Hobbit/editions",
                None,
            ),
            ("https://openlibrary.org/authors/OL26320A", None),
            ("https://www.goodreads.com/book/show/5907", None),
            (
                "https://www.tvmaze.com/shows/169",
                Some(Record::Tvmaze(169)),
            ),
            ("https://www.tvmaze.com/shows/169/episodes", None),
            ("https://www.tvmaze.com/shows/169/cast", None),
            (
                "https://www.tvmaze.com/shows/169/breaking-bad/seasons",
                None,
            ),
            (
                "https://www.imdb.com/title/tt0133093/",
                wd("P345", "tt0133093", Text::Record),
            ),
            ("https://www.imdb.com/title/tt0133093/reviews", None),
        ] {
            assert_eq!(record(uri), want, "{uri}");
        }
        assert_eq!(
            walled("https://www.imdb.com/title/tt0903747/"),
            Some(Walled::Imdb("tt0903747".into()))
        );
        assert_eq!(
            walled("https://pca.st/itunes/1296350485"),
            Some(Walled::ApplePodcast(1296350485))
        );
        // Overcast sends a reader to its sign-in page first: a wall.
        for overcast in [
            "https://overcast.fm/itunes1296350485",
            "https://overcast.fm/itunes1296350485/darknet-diaries",
        ] {
            assert_eq!(
                walled(overcast),
                Some(Walled::ApplePodcast(1296350485)),
                "{overcast}"
            );
        }
        for not in [
            "https://overcast.fm/itunes",
            "https://overcast.fm/+AbCdEf",
            "https://overcast.fm/itunes12x4",
        ] {
            assert_eq!(walled(not), None, "{not}");
        }
        assert_eq!(walled("https://www.imdb.com/title/tt0903747/reviews"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tmdb_page_is_named_by_the_item_that_carries_its_id() {
        let uri = "https://www.themoviedb.org/tv/1396";
        let search = wikidata::search_lookup("P4983", "1396").unwrap();
        let entities = wikidata::entities_lookup(&["Q1079".into()]);
        let (egress, recorded) = fixture::egress(vec![
            (
                &search,
                200,
                "application/json",
                read("search-P4983-1396.json"),
            ),
            (
                &entities,
                200,
                "application/json",
                read("entities-Q1079.json.gz"),
            ),
            (
                &labels_lookup("Q1079"),
                200,
                "application/json",
                read("labels-Q1079.json"),
            ),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let page = fixture::page(uri, "text/html", b"<title>x</title>".to_vec(), None);
        let doc = MediaRecords
            .page(&cx, uri, &page, &web_card(uri, "The Movie Database"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.name.as_deref(), Some("Breaking Bad"));
        assert_eq!(
            doc.description.as_deref(),
            Some("American crime drama television series (2008–2013)"),
            "the page's description was another language's"
        );
        assert!(doc.image.is_some(), "TMDB's poster stays");
        assert_eq!(doc.type_name, "TVSeries");
        assert_eq!(
            doc.attributes.get("wikidata").map(String::as_str),
            Some("Q1079")
        );
        assert_eq!(doc.source, "media_records");
        assert_eq!(recorded.asked().len(), 3);
        // An ID no item carries leaves the page's own card.
        let other = "https://www.themoviedb.org/tv/1";
        assert!(MediaRecords
            .page(&cx, other, &page, &web_card(other, "The Movie Database"))
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_shell_page_loses_the_service_s_text_and_image() {
        let item = crate::also_on::media::fixture::item("Q1079");
        let shell = web_card(
            "https://trakt.tv/shows/breaking-bad",
            "Trakt Web: Track Your Shows & Movies",
        );
        let doc = named_by_item(&shell, &item, "Breaking Bad".into(), None, Text::Shell);
        assert_eq!(doc.name.as_deref(), Some("Breaking Bad"));
        assert!(doc.description.is_none() && doc.image.is_none());
        let own = named_by_item(
            &shell,
            &item,
            "Breaking Bad".into(),
            Some("x".into()),
            Text::Own,
        );
        assert!(own.description.as_deref().unwrap().starts_with("Parhaan"));
    }

    /// A page the hook did not name takes the title the also-on sources found since (`type_card`
    /// asks this source, `apply_title`); one the hook named keeps the hook's card. The card's
    /// type is the link's, which the films source gives: with it registered, TVSeries; without,
    /// the card's own stays.
    #[test]
    fn a_page_the_hook_did_not_name_takes_the_title_the_sources_found() {
        use crate::also_on::media::{type_card, TypeHints};
        // A page the hook did not name takes the title the `titles` source found since.
        let named = TypeHints {
            title_type: Some("TVSeries".into()),
            title: Some("Breaking Bad".into()),
            title_description: Some("American crime drama television series".into()),
            ..Default::default()
        };
        let mut doc =
            crate::model::ResourceMeta::new("https://www.themoviedb.org/tv/1396", "jsonld", 60);
        doc.set_type("WebSite");
        doc.name = Some("The Movie Database".into());
        doc.description = Some("Parhaan miespuolisen draamanäyttelijän".into());
        type_card("https://www.themoviedb.org/tv/1396", &named, &mut doc);
        assert_eq!(doc.name.as_deref(), Some("Breaking Bad"));
        assert_eq!(
            doc.description.as_deref(),
            Some("American crime drama television series")
        );
        let films = crate::also_on::registered()
            .iter()
            .any(|s| s.id() == "films");
        assert_eq!(doc.type_name, if films { "TVSeries" } else { "WebSite" });
        // One the hook named keeps the hook's card.
        let mut hooked = crate::model::ResourceMeta::new(
            "https://www.themoviedb.org/tv/1396",
            "media_records",
            60,
        );
        hooked.name = Some("Breaking Bad (series)".into());
        type_card("https://www.themoviedb.org/tv/1396", &named, &mut hooked);
        assert_eq!(hooked.name.as_deref(), Some("Breaking Bad (series)"));
    }

    #[test]
    fn open_library_names_its_work_and_refuses_a_merged_one() {
        let raw = read("ol-work-OL27482W.json");
        let page = web_card(
            "https://openlibrary.org/works/OL27482W",
            "Ka hopita by J.R.R. Tolkien | Open Library",
        );
        let doc = named_by_open_library(&page, "works/OL27482W", &raw).unwrap();
        assert_eq!(doc.name.as_deref(), Some("The Hobbit"));
        assert!(doc.description.unwrap().starts_with("The Hobbit is a tale"));
        // A merged work's record redirects: it names another object.
        let merged = read("ol-work-OL893415W.json");
        assert!(named_by_open_library(&page, "works/OL893415W", &merged).is_none());
        // A record for another path names another object.
        assert!(named_by_open_library(&page, "works/OL1W", &raw).is_none());
    }

    #[test]
    fn tvmaze_names_its_show() {
        let page = web_card(
            "https://www.tvmaze.com/shows/169/breaking-bad",
            "Breaking Bad | TVmaze",
        );
        let doc = named_by_tvmaze(&page, 169, &read("tvmaze-show-169.json")).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Breaking Bad"));
        assert!(!doc.description.unwrap().contains('<'));
        assert!(doc.image.unwrap().starts_with("https://static.tvmaze.com/"));
        assert_eq!(doc.type_name, "TVSeries");
        assert!(named_by_tvmaze(&page, 170, &read("tvmaze-show-169.json")).is_none());
    }

    #[test]
    fn a_podcast_index_page_drops_the_site_s_name() {
        let page = web_card(
            "https://podcastindex.org/podcast/577105",
            "Darknet Diaries | Podcastindex.org",
        );
        let doc = named_by_podcast_index(&page).unwrap();
        assert_eq!(doc.name.as_deref(), Some("Darknet Diaries"));
        assert!(
            named_by_podcast_index(&web_card("https://podcastindex.org/podcast/1", "Plain"))
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_walled_player_page_is_named_by_itunes() {
        let raw = read("itunes-podcast-1584274529.json");
        let (egress, _) = fixture::egress(vec![]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = "https://pca.st/itunes/1584274529";
        let doc = itunes_card(&cx, uri, 1584274529, &raw).unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert!(doc.name.is_some() && doc.status == Status::Ok);
        assert!(itunes_card(&cx, uri, 1, &raw).is_none());
    }

    /// The walled page's card carries the show's latest episodes, read from the feed iTunes
    /// names for it (its first items, as recorded); a feed that does not answer leaves the card
    /// without a list (the stored one stays).
    #[tokio::test(start_paused = true)]
    async fn a_walled_player_pages_card_lists_the_shows_latest_episodes() {
        let lookup = "https://itunes.apple.com/lookup?id=1584274529&entity=podcast&country=us";
        let feed = "https://feeds.podcastindex.org/pc20.xml";
        assert_eq!(
            itunes_feed(&read("itunes-podcast-1584274529.json")).as_deref(),
            Some(feed)
        );
        let (egress, recorded) = fixture::egress(vec![
            (
                lookup,
                200,
                "text/javascript",
                read("itunes-podcast-1584274529.json"),
            ),
            (feed, 200, "application/xml", read("feed-pc20-head.xml")),
        ]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let uri = "https://pca.st/itunes/1584274529";
        let doc = MediaRecords.walled(&cx, uri).await.unwrap().unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(recorded.asked(), [lookup, feed]);
        let episodes = doc.episodes.expect("the feed's episodes");
        // Every item of the fixture's head (25, fewer than are kept).
        assert_eq!(episodes.len(), 25);
        let stream = episodes[0].stream.as_ref().expect("a public enclosure");
        assert!(stream.url.starts_with("https://"), "{stream:?}");
        assert_eq!(stream.media_type, "audio/mpeg");
        assert!(episodes[0].published.is_some());
        // The feed unanswered: the card, without a list.
        let (egress, _) = fixture::egress(vec![(
            lookup,
            200,
            "text/javascript",
            read("itunes-podcast-1584274529.json"),
        )]);
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = MediaRecords.walled(&cx, uri).await.unwrap().unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(doc.episodes, None);
    }

    /// The list is asked for within the read's deadline, and the card is handed back whatever the
    /// feed does: iTunes at once and the feed after seven seconds (no deadline set: the feed gets
    /// `FEED_WAIT`); two seconds left and the feed at 2.5 s; two seconds left and every parser
    /// busy for 6.5 s. Each time the card comes within the time left, without a list (the save
    /// then keeps the show's stored rows: `tests/episodes.rs`).
    #[tokio::test(start_paused = true)]
    async fn a_slow_feed_costs_the_list_not_the_card() {
        use std::time::Duration;
        let lookup = "https://itunes.apple.com/lookup?id=1584274529&entity=podcast&country=us";
        let feed = "https://feeds.podcastindex.org/pc20.xml";
        let uri = "https://pca.st/itunes/1584274529";
        let routes = || {
            fixture::egress(vec![
                (
                    lookup,
                    200,
                    "text/javascript",
                    read("itunes-podcast-1584274529.json"),
                ),
                (feed, 200, "application/xml", read("feed-pc20-head.xml")),
            ])
        };
        // No deadline of its own: the feed gets `FEED_WAIT`, and no more.
        let (egress, recorded) = routes();
        recorded.delay(feed, Duration::from_secs(7));
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let started = tokio::time::Instant::now();
        let doc = tokio::time::timeout(
            crate::sources::WALL_HOOK_TIMEOUT,
            MediaRecords.walled(&cx, uri),
        )
        .await
        .expect("the card within the hook's time")
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(doc.episodes, None, "no list from a feed that slow");
        assert_eq!(recorded.asked(), [lookup, feed], "the feed was asked");
        let waited = started.elapsed();
        assert!(
            waited >= crate::episodes::FEED_WAIT && waited < crate::sources::WALL_HOOK_TIMEOUT,
            "{waited:?}"
        );
        // Two seconds left of the read, the feed answering at 2.5 s: the card at once, no list.
        let (egress, recorded) = routes();
        recorded.delay(feed, Duration::from_millis(2500));
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(2);
        let doc = tokio::time::timeout_at(
            deadline,
            crate::sources::within(deadline, MediaRecords.walled(&cx, uri)),
        )
        .await
        .expect("the card within the read's deadline")
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(doc.episodes, None);
        assert_eq!(recorded.asked(), [lookup, feed]);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // Two seconds left, the feed at once, but the feed's parse waiting 6.5 s for a parser:
        // the wait is under the same deadline, so the card still comes in time, without a list.
        let (egress, recorded) = routes();
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_secs(2);
        let doc = tokio::time::timeout_at(
            deadline,
            crate::sources::within(
                deadline,
                crate::episodes::PARSE_WAIT
                    .scope(Duration::from_millis(6500), MediaRecords.walled(&cx, uri)),
            ),
        )
        .await
        .expect("the card within the read's deadline")
        .unwrap()
        .unwrap();
        assert_eq!(doc.type_name, "PodcastSeries");
        assert_eq!(doc.episodes, None, "no list while the parsers are busy");
        assert_eq!(recorded.asked(), [lookup, feed]);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // With time to spare, the same wait ends and the list comes.
        let (egress, _) = routes();
        let cx = Context {
            egress: &egress,
            ttl: 3600,
        };
        let doc = crate::episodes::PARSE_WAIT
            .scope(Duration::from_millis(1500), MediaRecords.walled(&cx, uri))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(doc.episodes.map(|l| l.len()), Some(25));
    }
}
