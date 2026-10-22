//! The `books` also-on source against its recorded answers (`common::media`).
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{books::Books, Fact, Found, Source},
    config::Config,
};

#[tokio::test]
async fn a_printed_book_and_its_work() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let s = subject("isbn:0743273567", None, None);
    assert!(Books.applies(&s));
    let found = Books.lookup(&cx, &s).await.unwrap();
    assert_eq!(
        keys(&found),
        [
            "openlibrary:edition:OL23109860M",
            "openlibrary:work:OL468431W"
        ]
    );
    // The work's Q-id is a fact: an edition reaches the item one way only, and the Open
    // Library work key already joins the editions.
    assert!(found.facts.contains(&Fact::new("wikidata", "Q214371")));
    assert_eq!(
        links(&found),
        [
            (
                "Open Library",
                "https://openlibrary.org/books/OL23109860M",
                "openlibrary"
            ),
            (
                "Open Library",
                "https://openlibrary.org/works/OL468431W",
                "openlibrary"
            ),
            (
                "Internet Archive",
                "https://archive.org/details/greatgat00fitz",
                "openlibrary"
            ),
            (
                "Goodreads",
                "https://www.goodreads.com/book/show/4671",
                "openlibrary"
            ),
            (
                "LibraryThing",
                "https://www.librarything.com/work/2964",
                "openlibrary"
            ),
            (
                "Google Books",
                "https://books.google.com/books?id=iXn5U2IzVH0C",
                "openlibrary"
            ),
            ("Amazon", "https://www.amazon.com/dp/0743273567", "isbn"),
            (
                "Goodreads",
                "https://www.goodreads.com/work/editions/245494",
                "wikidata"
            ),
            (
                "Project Gutenberg",
                "https://www.gutenberg.org/ebooks/64317",
                "wikidata"
            ),
        ]
    );
    assert!(
        found
            .links
            .iter()
            .filter(|l| l.via == "wikidata")
            .all(|l| l.kind.as_deref() == Some("work")),
        "the item's pages are the work's"
    );
    // Each link is named by the edition or the work it is a page of, so a tagged one joins.
    let edition = Some("openlibrary:edition:OL23109860M");
    let work = Some("openlibrary:work:OL468431W");
    for (uri, key) in [
        ("https://openlibrary.org/books/OL23109860M", edition),
        ("https://openlibrary.org/works/OL468431W", work),
        ("https://www.goodreads.com/book/show/4671", edition),
        ("https://www.librarything.com/work/2964", work),
        ("https://www.amazon.com/dp/0743273567", edition),
        ("https://www.goodreads.com/work/editions/245494", work),
    ] {
        assert_eq!(key_of(&found, uri), key, "{uri}");
    }
    assert!(
        !recorded.seen().iter().any(|u| u.contains("itunes")),
        "a printed ISBN is never asked of iTunes"
    );
}

#[tokio::test]
async fn a_work_link() {
    let cx = cx(Recorded::new(), Config::default());
    let hobbit = Books
        .lookup(
            &cx,
            &subject(
                "https://openlibrary.org/works/OL27482W/The_Hobbit",
                None,
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(keys(&hobbit), ["openlibrary:work:OL27482W"]);
    assert_eq!(hobbit.facts, [Fact::new("wikidata", "Q74287")]);
    assert!(links(&hobbit).contains(&(
        "Goodreads",
        "https://www.goodreads.com/work/editions/1540236",
        "wikidata"
    )));
    assert_eq!(
        key_of(&hobbit, "https://www.goodreads.com/work/editions/1540236"),
        Some("openlibrary:work:OL27482W")
    );
    assert!(
        hobbit.links.iter().all(|l| l.kind.is_none()),
        "the work itself"
    );
    // Dune's work has no Wikidata item naming it (the item names merged and wrong IDs): the key
    // only. A merged ID is followed to the work it is now.
    let dune = Books
        .lookup(
            &cx,
            &subject("https://openlibrary.org/works/OL893414W", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&dune), ["openlibrary:work:OL893414W"]);
    assert!(dune.links.is_empty() && dune.facts.is_empty());
    let merged = Books
        .lookup(
            &cx,
            &subject("https://openlibrary.org/works/OL893415W", None, None),
        )
        .await
        .unwrap();
    assert_eq!(keys(&merged), ["openlibrary:work:OL893414W"]);
    assert_eq!(
        links(&merged),
        [(
            "Open Library",
            "https://openlibrary.org/works/OL893414W",
            "openlibrary"
        )]
    );
    assert_eq!(
        merged.links[0].key.as_deref(),
        Some("openlibrary:work:OL893414W")
    );
}

#[tokio::test]
async fn an_ebook_on_apple_books() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Books
        .lookup(&cx, &subject("isbn:9780804139038", None, None))
        .await
        .unwrap();
    assert!(links(&found).contains(&(
        "Apple Books",
        "https://books.apple.com/us/book/id721429965",
        "itunes"
    )));
    assert!(
        key_of(&found, "https://books.apple.com/us/book/id721429965")
            .is_some_and(|k| k.starts_with("openlibrary:edition:")),
        "found by the edition's ISBN"
    );
    assert!(!found.links.iter().any(|l| l.site == "Amazon"));
}

#[tokio::test]
async fn an_unknown_isbn_finds_nothing() {
    let recorded = Recorded::new();
    let cx = cx(recorded.clone(), Config::default());
    let found = Books
        .lookup(&cx, &subject("isbn:9798999999993", None, None))
        .await
        .unwrap();
    assert_eq!(found, Found::default());
    assert_eq!(recorded.seen().len(), 1);
}
