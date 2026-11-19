//! The `films` also-on source against its recorded answers (`common::media`).
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{films::Films, Fact, Source},
    config::Config,
};

/// Letterboxd's film page, where its `imdb/` redirect for The Shawshank Redemption ends.
const SHAWSHANK_PAGE: (&str, u16, &str) = (
    "https://letterboxd.com/film/the-shawshank-redemption/",
    200,
    "<html><head><title>The Shawshank Redemption (1994)</title></head><body></body></html>",
);

#[tokio::test]
async fn a_film_by_its_imdb_link() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let s = subject("https://www.imdb.com/title/tt0111161/", None, None);
    assert!(Films.applies(&s));
    let found = Films.lookup(&cx, &s).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt0111161"]);
    assert_eq!(found.keys[0].via, "imdb");
    assert!(found.facts.contains(&Fact::new("wikidata", "Q172241")));
    assert!(found.facts.contains(&Fact::new("title_type", "Movie")));
    assert_eq!(
        sites(&found),
        [
            "TMDB",
            "Letterboxd",
            "Trakt",
            "Rotten Tomatoes",
            "TheTVDB",
            "Apple TV",
            "Netflix",
            "MUBI"
        ],
        "never the card's own service"
    );
    let seen = recorded.seen();
    assert!(
        !seen.iter().any(|u| u.contains("letterboxd.com")),
        "the item has its Letterboxd page"
    );
    // A film is no show on TVmaze.
    assert!(seen.contains(&"https://api.tvmaze.com/lookup/shows?imdb=tt0111161".to_string()));
    assert!(!sites(&found).contains(&"TVmaze"));
    assert!(
        found
            .links
            .iter()
            .all(|l| l.key.as_deref() == Some("imdb:tt0111161")),
        "every link is the title's, named through its IMDb ID"
    );
}

#[tokio::test]
async fn a_film_by_its_tmdb_link_reaches_its_imdb_id_through_wikidata() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Films
        .lookup(
            &cx,
            &subject(
                "https://www.themoviedb.org/movie/278-the-shawshank-redemption",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt0111161"]);
    assert_eq!(found.keys[0].via, "wikidata");
    assert!(sites(&found).contains(&"IMDb"));
    assert!(found
        .links
        .iter()
        .all(|l| l.key.as_deref() == Some("imdb:tt0111161")));
    assert!(!sites(&found).contains(&"TMDB"));
}

#[tokio::test]
async fn a_netflix_link_reaches_its_title_through_wikidata() {
    let cx = cx(Recorded::new(), Config::default());
    let uri = "https://www.netflix.com/title/70143836";
    let found = Films.lookup(&cx, &subject(uri, None, None)).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt0903747"], "{uri}");
    assert_eq!(found.keys[0].via, "wikidata");
    assert!(found.facts.contains(&Fact::new("title_type", "TVSeries")));
}

#[tokio::test]
async fn a_series_by_its_tvmaze_link() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Films
        .lookup(
            &cx,
            &subject("https://www.tvmaze.com/shows/169/breaking-bad", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt0903747"]);
    assert_eq!(found.keys[0].via, "tvmaze");
    assert!(!sites(&found).contains(&"TVmaze"));
    assert!(sites(&found).contains(&"IMDb"));
    // Letterboxd has no film for a series: its imdb/ page answers "not found".
    assert!(recorded
        .seen()
        .contains(&"https://letterboxd.com/imdb/tt0903747/".to_string()));
    assert!(!sites(&found).contains(&"Letterboxd"));
}

#[tokio::test]
async fn letterboxd_and_tvmaze_by_imdb_id() {
    // An IMDb ID Wikidata has no item for: Letterboxd's redirect names the film exactly.
    let tt = "https://www.imdb.com/title/tt9999999/";
    let recorded = Recorded::with(&[
        (
            "https://letterboxd.com/imdb/tt9999999/",
            302,
            "/film/the-shawshank-redemption/",
        ),
        SHAWSHANK_PAGE,
    ]);
    let cx = cx(recorded.clone(), Config::default());
    let found = Films.lookup(&cx, &subject(tt, None, None)).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt9999999"]);
    assert_eq!(
        links(&found),
        [(
            "Letterboxd",
            "https://letterboxd.com/film/the-shawshank-redemption/",
            "letterboxd"
        )]
    );
    // Without the redirect, and with TVmaze's 404, nothing.
    let found = Films
        .lookup(
            &crate::cx(Recorded::new(), Config::default()),
            &subject(tt, None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&found), ["imdb:tt9999999"]);
    assert!(found.links.is_empty());
}

/// A Rotten Tomatoes page (the owner's Halloween preview) reaches its title through the one
/// Wikidata item with its Rotten Tomatoes ID: the item's IMDb ID is the key, and its other pages
/// (never Rotten Tomatoes) are links named through it.
#[tokio::test]
async fn a_rotten_tomatoes_page_reaches_its_title_through_wikidata() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let s = subject(
        "https://www.rottentomatoes.com/m/1009113-halloween",
        None,
        None,
    );
    assert!(Films.applies(&s));
    let found = Films.lookup(&cx, &s).await.unwrap();
    assert_eq!(keys(&found), ["imdb:tt0077651"]);
    assert_eq!(found.keys[0].via, "wikidata");
    assert!(found.facts.contains(&Fact::new("wikidata", "Q221103")));
    let sites = sites(&found);
    for site in ["IMDb", "Letterboxd"] {
        assert!(sites.contains(&site), "{site} in {sites:?}");
    }
    assert!(!sites.contains(&"Rotten Tomatoes"), "never its own service");
    assert!(found
        .links
        .iter()
        .all(|l| l.key.as_deref() == Some("imdb:tt0077651")));
    assert!(recorded
        .seen()
        .iter()
        .any(|u| u.contains("haswbstatement%3AP1258%3Dm%2F1009113-halloween")));
}

/// A Rotten Tomatoes ID two items carry at their best rank (Chungking Express, and a film whose
/// item names it too) is ambiguous: no item, so no key and no links.
#[tokio::test]
async fn an_id_two_items_carry_joins_nothing() {
    let cx = cx(Recorded::new(), Config::default());
    let s = subject(
        "https://www.rottentomatoes.com/m/chungking_express",
        None,
        None,
    );
    assert!(Films.applies(&s));
    let found = Films.lookup(&cx, &s).await.unwrap();
    assert!(found.keys.is_empty(), "{:?}", found.keys);
    assert!(found.links.is_empty(), "{:?}", found.links);
}

/// An item over its read's cap is no answer: the page finds no title, and the lookup succeeds
/// rather than failing and coming back to read the same megabytes.
#[tokio::test]
async fn an_item_over_its_cap_is_no_answer() {
    let cx = cx(
        Recorded::with_too_large(&["action=wbgetentities"]),
        Config::default(),
    );
    let s = subject(
        "https://www.rottentomatoes.com/m/1009113-halloween",
        None,
        None,
    );
    let found = Films.lookup(&cx, &s).await.unwrap();
    assert!(found.keys.is_empty() && found.links.is_empty(), "{found:?}");
}
