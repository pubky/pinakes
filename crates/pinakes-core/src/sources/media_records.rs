//! Media pages named after the object they show. A film's, series', book's or podcast's page on a
//! service often names the site, another language's title, or nothing at all (an app shell): the
//! card then takes the object's name, and where the page's own text is not the object's, its
//! description, from the object's exact record. A record is exact: the Wikidata item that carries
//! the page's own ID on that service, the service's own record by that ID, never a search by
//! name. A page hook (and, for walled pages, a wall hook): an error leaves the page's own card.
use super::{Context, Example, Source};
use crate::{
    also_on::films,
    hubs::wikidata,
    model::{ResourceMeta, Status},
    resolver::egress::{FetchOptions, Fetched},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

pub struct MediaRecords;

/// What a page's own text is worth once the record names its object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
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
        ("imdb.com" | "m.imdb.com", ["title", tt]) if films::imdb(tt) => {
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
        ("tvmaze.com", ["shows", id, ..]) if digits(id) && s.len() <= 3 => {
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
        ("www.imdb.com" | "imdb.com" | "m.imdb.com", ["title", tt]) if films::imdb(tt) => {
            Some(Walled::Imdb((*tt).to_owned()))
        }
        ("pca.st" | "castro.fm", ["itunes", id]) => id.parse().ok().map(Walled::ApplePodcast),
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
    let text = |key: &str| {
        entity[key]["en"]["value"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Some((text("labels")?, text("descriptions")))
}

fn json() -> FetchOptions {
    FetchOptions {
        accept: Some("application/json".into()),
        ..Default::default()
    }
}

/// The item carrying `property` = `value`, and its English label and description.
async fn item(
    cx: &Context<'_>,
    property: &str,
    value: &str,
) -> Result<Option<(wikidata::Item, String, Option<String>)>> {
    let wikidata::Found::One(item) = wikidata::item_with(cx.egress, property, value).await? else {
        return Ok(None);
    };
    let f = cx
        .egress
        .discover(&labels_lookup(&item.qid), &json(), 86400)
        .await?;
    if f.status != 200 {
        return Ok(None);
    }
    Ok(parse_labels(&f.body, &item.qid).map(|(label, description)| (*item, label, description)))
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
    doc.name = Some(label);
    if text != Text::Own {
        doc.description = description;
    }
    if text == Text::Shell {
        doc.image = None;
    }
    if let Some(kind) = films::title_type(item) {
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
            } => item(cx, property, &value)
                .await?
                .map(|(item, label, description)| {
                    named_by_item(doc, &item, label, description, text)
                }),
            Record::OpenLibrary(path) => {
                let f = cx
                    .egress
                    .discover(
                        &format!("https://openlibrary.org/{path}.json"),
                        &json(),
                        3600,
                    )
                    .await?;
                (f.status == 200)
                    .then(|| named_by_open_library(doc, &path, &f.body))
                    .flatten()
            }
            Record::Tvmaze(id) => {
                let f = cx
                    .egress
                    .discover(&format!("https://api.tvmaze.com/shows/{id}"), &json(), 3600)
                    .await?;
                (f.status == 200)
                    .then(|| named_by_tvmaze(doc, id, &f.body))
                    .flatten()
            }
            Record::PodcastIndex => named_by_podcast_index(doc),
        })
    }
    async fn walled(&self, cx: &Context<'_>, uri: &str) -> Result<Option<ResourceMeta>> {
        Ok(match walled(uri) {
            Some(Walled::Imdb(tt)) => {
                item(cx, "P345", &tt)
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
                    ..json()
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
                (f.status == 200)
                    .then(|| itunes_card(cx, uri, show, &f.body))
                    .flatten()
            }
            None => None,
        })
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

    #[test]
    fn open_library_names_its_work_and_refuses_a_merged_one() {
        let raw = crate::also_on::media::fixture::read("books", "ol-work-OL27482W.json");
        let page = web_card(
            "https://openlibrary.org/works/OL27482W",
            "Ka hopita by J.R.R. Tolkien | Open Library",
        );
        let doc = named_by_open_library(&page, "works/OL27482W", &raw).unwrap();
        assert_eq!(doc.name.as_deref(), Some("The Hobbit"));
        assert!(doc.description.unwrap().starts_with("The Hobbit is a tale"));
        // A merged work's record redirects: it names another object.
        let merged = crate::also_on::media::fixture::read("books", "ol-work-OL893415W.json");
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
        let raw =
            crate::also_on::media::fixture::read("podcasts", "itunes-podcast-1584274529.json");
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
}
