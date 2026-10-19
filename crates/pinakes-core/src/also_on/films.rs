//! Films and series: a title by its IMDb, TMDB, Letterboxd or TVmaze link, and where else it is.
//!
//! The IMDb title ID (`tt…`) is the join: an IMDb link names it, TVmaze's record of a show names
//! it, and a TMDB or Letterboxd link reaches it through the one Wikidata item with that link's
//! ID. The item (by its IMDb ID) gives the title's other pages. Letterboxd's `imdb/{tt}/`
//! redirect names its film exactly (an unknown ID answers a page, not a redirect), and TVmaze's
//! `lookup/shows?imdb=` its show (TVmaze's data is CC BY-SA: its links say `via: "tvmaze"`);
//! each is asked only when the item has no link there.
use super::{
    media::{self, digits, property, slug, Property},
    Context, Found, Key, Link, Source, Subject,
};
use crate::{
    hubs::{self, wikidata},
    resolver::egress::FetchOptions,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;

/// What a link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Film {
    /// An IMDb title (`tt` and 7 or 8 digits).
    Imdb(String),
    /// A TMDB movie, or (`tv`) series.
    Tmdb { tv: bool, id: String },
    /// A Letterboxd film, by its slug.
    Letterboxd(String),
    /// A TVmaze show.
    Tvmaze(u64),
}

/// An IMDb title ID.
pub fn imdb(v: &str) -> bool {
    v.strip_prefix("tt")
        .is_some_and(|d| matches!(d.len(), 7 | 8) && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The title a link names: `imdb.com/title/{tt}`, `themoviedb.org/movie/{id}` or `/tv/{id}`
/// (with or without the slug after the ID), `letterboxd.com/film/{slug}/` and
/// `tvmaze.com/shows/{id}`.
pub fn film(uri: &str) -> Option<Film> {
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (host, s.as_slice()) {
        ("imdb.com" | "m.imdb.com", ["title", tt, ..]) if imdb(tt) => {
            Some(Film::Imdb((*tt).to_owned()))
        }
        ("themoviedb.org", [kind @ ("movie" | "tv"), id, ..]) => {
            let id = id.split('-').next()?;
            digits(id).then(|| Film::Tmdb {
                tv: *kind == "tv",
                id: id.to_owned(),
            })
        }
        ("letterboxd.com", ["film", name]) if slug(name) => {
            Some(Film::Letterboxd((*name).to_owned()))
        }
        ("tvmaze.com", ["shows", id, ..]) if digits(id) => id.parse().ok().map(Film::Tvmaze),
        _ => None,
    }
}

/// The service a link is on, as its links name it.
fn site(film: &Film) -> &'static str {
    match film {
        Film::Imdb(_) => "IMDb",
        Film::Tmdb { .. } => "TMDB",
        Film::Letterboxd(_) => "Letterboxd",
        Film::Tvmaze(_) => "TVmaze",
    }
}

/// The Wikidata property holding a link's own ID, and the ID.
fn property_of(film: &Film) -> (&'static str, String) {
    match film {
        Film::Imdb(tt) => ("P345", tt.clone()),
        Film::Tmdb { tv: false, id } => ("P4947", id.clone()),
        Film::Tmdb { tv: true, id } => ("P4983", id.clone()),
        Film::Letterboxd(name) => ("P6127", name.clone()),
        Film::Tvmaze(id) => ("P8600", id.to_string()),
    }
}

/// Letterboxd's film for an IMDb ID: it redirects to the film's page.
pub fn letterboxd_request(tt: &str) -> String {
    format!("https://letterboxd.com/imdb/{tt}/")
}
/// The film page a Letterboxd answer ended on, if it redirected to one (`final_url` is where
/// the request ended).
pub fn parse_letterboxd(status: u16, final_url: &url::Url) -> Option<Link> {
    if status != 200 || final_url.host_str() != Some("letterboxd.com") {
        return None;
    }
    let s: Vec<&str> = final_url
        .path_segments()?
        .filter(|s| !s.is_empty())
        .collect();
    match s.as_slice() {
        ["film", name] if slug(name) => Some(Link::new(
            format!("https://letterboxd.com/film/{name}/"),
            "Letterboxd",
            "letterboxd",
        )),
        _ => None,
    }
}

/// TVmaze's show for an IMDb ID (it redirects to the show's record).
pub fn tvmaze_lookup(tt: &str) -> String {
    format!("https://api.tvmaze.com/lookup/shows?imdb={tt}")
}
/// TVmaze's record of a show.
pub fn tvmaze_show(id: u64) -> String {
    format!("https://api.tvmaze.com/shows/{id}")
}
/// A show as TVmaze records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Show {
    pub id: u64,
    pub imdb: Option<String>,
}
/// The show, or `None` when TVmaze has none (404).
pub fn parse_tvmaze(status: u16, raw: &[u8]) -> Result<Option<Show>> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(None);
    };
    Ok(Some(Show {
        id: v["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .context("no show id")?,
        imdb: v["externals"]["imdb"]
            .as_str()
            .filter(|tt| imdb(tt))
            .map(str::to_owned),
    }))
}
fn tvmaze_link(id: u64) -> Link {
    Link::new(
        format!("https://www.tvmaze.com/shows/{id}"),
        "TVmaze",
        "tvmaze",
    )
}

/// Trakt's path IDs: `movies/{slug}` or `shows/{slug}`.
fn trakt(v: &str) -> bool {
    v.split_once('/').is_some_and(|(kind, name)| {
        matches!(kind, "movies" | "shows")
            && !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}
/// Rotten Tomatoes' film and series paths: `m/{name}` or `tv/{name}`.
fn rotten(v: &str) -> bool {
    v.split_once('/').is_some_and(|(kind, name)| {
        matches!(kind, "m" | "tv")
            && (1..=200).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'\''))
    })
}
/// Apple TV's content IDs (`umc.cmc.…`).
fn apple_tv(v: &str) -> bool {
    v.strip_prefix("umc.cmc.").is_some_and(|id| {
        (22..=25).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// The title's pages, from its Wikidata item. One link per service: the first property in the
/// table with exactly one value wins.
pub const WIKIDATA: &[Property] = &[
    property("P345", "IMDb", "https://www.imdb.com/title/$1/", imdb),
    property(
        "P4947",
        "TMDB",
        "https://www.themoviedb.org/movie/$1",
        digits,
    ),
    property("P4983", "TMDB", "https://www.themoviedb.org/tv/$1", digits),
    property(
        "P6127",
        "Letterboxd",
        "https://letterboxd.com/film/$1/",
        slug,
    ),
    property("P8013", "Trakt", "https://trakt.tv/$1", trakt),
    property("P12492", "Trakt", "https://trakt.tv/movies/$1", digits),
    property(
        "P1258",
        "Rotten Tomatoes",
        "https://www.rottentomatoes.com/$1",
        rotten,
    ),
    property(
        "P12196",
        "TheTVDB",
        "https://thetvdb.com/dereferrer/movie/$1",
        digits,
    ),
    property(
        "P4835",
        "TheTVDB",
        "https://thetvdb.com/dereferrer/series/$1",
        digits,
    ),
    property(
        "P9586",
        "Apple TV",
        "https://tv.apple.com/movie/$1",
        apple_tv,
    ),
    property(
        "P9751",
        "Apple TV",
        "https://tv.apple.com/show/$1",
        apple_tv,
    ),
    property(
        "P1874",
        "Netflix",
        "https://www.netflix.com/title/$1",
        |v| (6..=8).contains(&v.len()) && digits(v),
    ),
    property("P7299", "MUBI", "https://mubi.com/films/$1", digits),
    property("P8600", "TVmaze", "https://www.tvmaze.com/shows/$1", digits),
    property(
        "P4086",
        "MyAnimeList",
        "https://myanimelist.net/anime/$1",
        digits,
    ),
    property("P8729", "AniList", "https://anilist.co/anime/$1", digits),
];

/// The item's one IMDb title ID.
fn item_imdb(item: &wikidata::Item) -> Option<String> {
    match item.id("P345") {
        [tt] if imdb(tt) => Some(tt.clone()),
        _ => None,
    }
}

/// The source: Wikidata, Letterboxd and TVmaze.
pub struct Films;

impl Films {
    async fn item(cx: &Context, property: &str, id: &str) -> Result<Option<wikidata::Item>> {
        Ok(match wikidata::item_with(&cx.egress, property, id).await? {
            wikidata::Found::One(item) => Some(*item),
            _ => None,
        })
    }
    async fn tvmaze(cx: &Context, url: &str) -> Result<Option<Show>> {
        let options = FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        };
        let f = cx.egress.discover(url, &options, 3600).await?;
        parse_tvmaze(f.status, &f.body)
    }
    /// Letterboxd's film for the title; a refusal (e.g. a challenge page) is no answer, a rate
    /// limit or server error a retry.
    async fn letterboxd(cx: &Context, tt: &str) -> Result<Option<Link>> {
        match cx
            .egress
            .discover(&letterboxd_request(tt), &FetchOptions::default(), 3600)
            .await
        {
            Ok(f) if f.status >= 500 => Err(crate::resolver::RetryLater {
                class: "upstream",
                seconds: 60,
            }
            .into()),
            Ok(f) => Ok(parse_letterboxd(f.status, &f.url)),
            Err(e) if e.is::<crate::resolver::RetryLater>() => Err(e),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "letterboxd gave no answer");
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl Source for Films {
    fn id(&self) -> &'static str {
        "films"
    }
    fn applies(&self, subject: &Subject) -> bool {
        film(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        let Some(film) = film(&subject.uri) else {
            return Ok(Found::default());
        };
        let mut found = Found::default();
        // The title's IMDb ID, and its item.
        let (tt, item) = match &film {
            Film::Imdb(tt) => (Some(tt.clone()), Self::item(cx, "P345", tt).await?),
            Film::Tvmaze(id) => {
                let show = Self::tvmaze(cx, &tvmaze_show(*id))
                    .await?
                    .filter(|s| s.id == *id);
                match show.and_then(|s| s.imdb) {
                    Some(tt) => {
                        let item = Self::item(cx, "P345", &tt).await?;
                        (Some(tt), item)
                    }
                    None => (None, None),
                }
            }
            other => {
                let (property, id) = property_of(other);
                let item = Self::item(cx, property, &id).await?;
                (item.as_ref().and_then(item_imdb), item)
            }
        };
        if let Some(tt) = &tt {
            found.keys.push(Key {
                key: format!("imdb:{tt}"),
                via: if matches!(film, Film::Imdb(_)) {
                    "imdb"
                } else if matches!(film, Film::Tvmaze(_)) {
                    "tvmaze"
                } else {
                    "wikidata"
                }
                .into(),
            });
        }
        if let Some(item) = &item {
            found
                .keys
                .extend(hubs::key::wikidata(&item.qid).map(|key| Key {
                    key,
                    via: "wikidata".into(),
                }));
            found.links = media::links(item, WIKIDATA);
        }
        let Some(tt) = tt else {
            found.links.retain(|l| l.site != site(&film));
            return Ok(found);
        };
        // A link's own service is the card itself.
        let own = site(&film);
        found.links.retain(|l| l.site != own);
        let wanted = |found: &Found, s: &str| s != own && !found.links.iter().any(|l| l.site == s);
        if wanted(&found, "Letterboxd") {
            found.links.extend(Self::letterboxd(cx, &tt).await?);
        }
        if wanted(&found, "TVmaze") {
            if let Some(show) = Self::tvmaze(cx, &tvmaze_lookup(&tt)).await? {
                if show.imdb.as_deref() == Some(tt.as_str()) {
                    found.links.push(tvmaze_link(show.id));
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::also_on::media::fixture;

    fn read(name: &str) -> Vec<u8> {
        fixture::read("films", name)
    }

    #[test]
    fn links_that_name_a_title() {
        let shawshank = Some(Film::Imdb("tt0111161".into()));
        for uri in [
            "https://www.imdb.com/title/tt0111161/",
            "https://m.imdb.com/title/tt0111161/reviews",
            "http://imdb.com/title/tt0111161",
        ] {
            assert_eq!(film(uri), shawshank, "{uri}");
        }
        assert_eq!(
            film("https://www.themoviedb.org/movie/278-the-shawshank-redemption"),
            Some(Film::Tmdb {
                tv: false,
                id: "278".into()
            })
        );
        assert_eq!(
            film("https://www.themoviedb.org/tv/1396"),
            Some(Film::Tmdb {
                tv: true,
                id: "1396".into()
            })
        );
        assert_eq!(
            film("https://letterboxd.com/film/the-shawshank-redemption/"),
            Some(Film::Letterboxd("the-shawshank-redemption".into()))
        );
        assert_eq!(
            film("https://www.tvmaze.com/shows/169/breaking-bad"),
            Some(Film::Tvmaze(169))
        );
        for uri in [
            "https://www.imdb.com/name/nm0000151/",
            "https://www.imdb.com/title/tt011116/",
            "https://www.imdb.com:8443/title/tt0111161/",
            "https://letterboxd.com/film/the-shawshank-redemption/reviews/",
            "https://letterboxd.com/imdb/tt0111161/",
            "https://www.themoviedb.org/person/504",
            "https://www.tvmaze.com/people/1",
        ] {
            assert_eq!(film(uri), None, "{uri}");
        }
    }

    #[test]
    fn letterboxd_redirects_only_for_a_film_it_has() {
        assert_eq!(
            letterboxd_request("tt0111161"),
            "https://letterboxd.com/imdb/tt0111161/"
        );
        let ended = |u: &str| url::Url::parse(u).unwrap();
        let link = parse_letterboxd(
            200,
            &ended("https://letterboxd.com/film/the-shawshank-redemption/"),
        )
        .unwrap();
        assert_eq!(
            (link.site.as_str(), link.uri.as_str(), link.via.as_str()),
            (
                "Letterboxd",
                "https://letterboxd.com/film/the-shawshank-redemption/",
                "letterboxd"
            )
        );
        // An unknown ID answers its own page ("IMDb ID Not found"), not a redirect.
        assert!(
            String::from_utf8_lossy(&read("letterboxd-imdb-missing.html.gz"))
                .contains("IMDb ID Not found")
        );
        assert_eq!(
            parse_letterboxd(200, &ended("https://letterboxd.com/imdb/tt9999999/")),
            None
        );
        assert_eq!(
            parse_letterboxd(404, &ended("https://letterboxd.com/film/x/")),
            None
        );
        assert_eq!(
            parse_letterboxd(200, &ended("https://example.org/film/x/")),
            None
        );
    }

    #[test]
    fn tvmaze_names_the_shows_imdb_id() {
        assert_eq!(
            tvmaze_lookup("tt0903747"),
            "https://api.tvmaze.com/lookup/shows?imdb=tt0903747"
        );
        assert_eq!(
            parse_tvmaze(200, &read("tvmaze-imdb-tt0903747-followed.json")).unwrap(),
            Some(Show {
                id: 169,
                imdb: Some("tt0903747".into())
            })
        );
        assert_eq!(
            parse_tvmaze(404, &read("tvmaze-imdb-missing.json")).unwrap(),
            None
        );
        assert!(parse_tvmaze(500, b"",).is_err());
    }

    #[test]
    fn a_film_and_a_series_on_wikidata() {
        let sites = |qid: &str| -> Vec<(String, String)> {
            media::links(&fixture::item(qid), WIKIDATA)
                .into_iter()
                .map(|l| (l.site, l.uri))
                .collect()
        };
        let film = sites("Q172241");
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            film,
            [
                s("IMDb", "https://www.imdb.com/title/tt0111161/"),
                s("TMDB", "https://www.themoviedb.org/movie/278"),
                s(
                    "Letterboxd",
                    "https://letterboxd.com/film/the-shawshank-redemption/"
                ),
                s(
                    "Trakt",
                    "https://trakt.tv/movies/the-shawshank-redemption-1994"
                ),
                s(
                    "Rotten Tomatoes",
                    "https://www.rottentomatoes.com/m/shawshank_redemption"
                ),
                s("TheTVDB", "https://thetvdb.com/dereferrer/movie/190"),
                s(
                    "Apple TV",
                    "https://tv.apple.com/movie/umc.cmc.459n4f98t82t8ommdoa7ebnny"
                ),
                s("Netflix", "https://www.netflix.com/title/70005379"),
                s("MUBI", "https://mubi.com/films/3403"),
            ]
        );
        let series = sites("Q1079");
        assert!(series.contains(&s("TMDB", "https://www.themoviedb.org/tv/1396")));
        assert!(
            series.contains(&s(
                "Rotten Tomatoes",
                "https://www.rottentomatoes.com/tv/breaking_bad"
            )),
            "the deprecated value is left out"
        );
        assert!(series.contains(&s("TheTVDB", "https://thetvdb.com/dereferrer/series/81189")));
        assert!(series.contains(&s("TVmaze", "https://www.tvmaze.com/shows/169")));
        let anime = sites("Q101244908");
        assert!(anime.contains(&s("MyAnimeList", "https://myanimelist.net/anime/1")));
        assert!(anime.contains(&s("AniList", "https://anilist.co/anime/1")));
        assert!(trakt("shows/breaking-bad") && !trakt("people/x") && !trakt("movies/"));
        assert!(rotten("m/shawshank_redemption") && !rotten("celebrity/x"));
    }
}
