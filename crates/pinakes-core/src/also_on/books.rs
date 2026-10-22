//! Books: an edition by ISBN (or its Open Library page), or a work by its Open Library page, and
//! where else it is.
//!
//! Open Library's edition record is the authority: the edition must list the ISBN asked. Its
//! work joins the editions of one work as a group (a hint, like every group). Links come only
//! from exact IDs: the edition's own Open Library, Internet Archive (`ocaid`), Goodreads,
//! LibraryThing and Google Books IDs (each only when the record holds exactly one), Amazon's
//! `dp/{ISBN-10}` for a printed 978 ISBN, Apple Books for an ebook edition whose ISBN iTunes
//! knows exactly, and the work's Wikidata item. Goodreads' own `book/isbn/{isbn}` redirect is
//! never used: it answers an unknown ISBN with an unrelated book.
use super::{
    media::{self, digits, property, slug, uuid, Deferred, Property},
    Context, Fact, Found, Key, Link, Source, Subject,
};
use crate::{
    hubs::wikidata,
    resolver::{cards, egress::FetchOptions},
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

const OPEN_LIBRARY: &str = "https://openlibrary.org";

/// What a link names: an edition by ISBN-13, or by its Open Library edition ID, or a work by
/// its Open Library work ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Book {
    Isbn(String),
    Edition(String),
    Work(String),
}

/// An Open Library ID of one kind: `OL`, digits, then `M` (edition) or `W` (work).
fn ol_id(value: &str, kind: char) -> Option<String> {
    let digits = value.strip_prefix("OL")?.strip_suffix(kind)?;
    media::digits(digits).then(|| value.to_owned())
}

/// The book a link names: `isbn:…`, `openlibrary.org/isbn/{isbn}`,
/// `openlibrary.org/books/{OL…M}` or `openlibrary.org/works/{OL…W}` (any title slug after it).
pub fn book(uri: &str) -> Option<Book> {
    if uri.starts_with("isbn:") {
        return cards::isbn(uri).map(|n| Book::Isbn(cards::isbn13(&n)));
    }
    let url = url::Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "https" | "http")
        || !matches!(url.host_str()?, "openlibrary.org" | "www.openlibrary.org")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let s: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match s.as_slice() {
        ["isbn", n] => {
            let n = n.strip_suffix(".json").unwrap_or(n);
            cards::isbn(&format!("isbn:{n}")).map(|n| Book::Isbn(cards::isbn13(&n)))
        }
        ["books", id, ..] => ol_id(id.strip_suffix(".json").unwrap_or(id), 'M').map(Book::Edition),
        ["works", id, ..] => ol_id(id.strip_suffix(".json").unwrap_or(id), 'W').map(Book::Work),
        _ => None,
    }
}

/// A work's Open Library record.
pub fn work_request(id: &str) -> String {
    format!("{OPEN_LIBRARY}/works/{id}.json")
}
/// What Open Library's record of a work ID says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkRecord {
    /// The work, under the ID asked.
    Work,
    /// The ID was merged into this work.
    Redirect(String),
    /// No such work (unknown or deleted).
    Gone,
}
/// The record of work `id`: it must be that work, or a redirect to another work.
pub fn parse_work(status: u16, raw: &[u8], id: &str) -> Result<WorkRecord> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(WorkRecord::Gone);
    };
    let key = |field: &str| {
        v[field]
            .as_str()
            .and_then(|k| k.strip_prefix("/works/"))
            .and_then(|k| ol_id(k, 'W'))
    };
    Ok(match v["type"]["key"].as_str() {
        Some("/type/work") => {
            anyhow::ensure!(
                key("key").as_deref() == Some(id),
                "Open Library answered for another work"
            );
            WorkRecord::Work
        }
        Some("/type/redirect") => {
            WorkRecord::Redirect(key("location").context("no redirect target")?)
        }
        Some("/type/delete") => WorkRecord::Gone,
        other => anyhow::bail!("unexpected work record type {other:?}"),
    })
}

/// The edition's Open Library record (a by-ISBN request redirects to it).
pub fn edition_request(book: &Book) -> String {
    match book {
        Book::Isbn(isbn) => format!("{OPEN_LIBRARY}/isbn/{isbn}.json"),
        Book::Edition(id) => format!("{OPEN_LIBRARY}/books/{id}.json"),
        Book::Work(id) => work_request(id),
    }
}

/// An edition as Open Library records it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edition {
    pub id: String,
    pub work: Option<String>,
    pub isbns: Vec<String>,
    pub ocaid: Option<String>,
    pub ebook: bool,
    pub goodreads: Option<String>,
    pub librarything: Option<String>,
    pub google: Option<String>,
}

/// The one value of a list field, when it holds exactly one.
fn single(v: &Value) -> Option<String> {
    let mut values: Vec<&str> = v
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    values.dedup();
    match values.as_slice() {
        [one] => Some((*one).to_owned()),
        _ => None,
    }
}

/// An ebook's format as Open Library writes it (`Ebook`, `Epub`, `Electronic resource`, …).
fn ebook(format: &str) -> bool {
    let f = format.trim().to_ascii_lowercase();
    matches!(
        f.as_str(),
        "ebook"
            | "e-book"
            | "epub"
            | "electronic resource"
            | "electronic book"
            | "kindle"
            | "kindle edition"
    )
}

/// The edition, or `None` when Open Library has none (404). It must be the edition asked: the
/// ISBN among its ISBNs, or the edition ID its own.
pub fn parse_edition(status: u16, raw: &[u8], book: &Book) -> Result<Option<Edition>> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(None);
    };
    let id = v["key"]
        .as_str()
        .and_then(|k| k.strip_prefix("/books/"))
        .and_then(|k| ol_id(k, 'M'))
        .context("no edition key")?;
    let isbns: Vec<String> = ["isbn_13", "isbn_10"]
        .iter()
        .flat_map(|f| v[*f].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter_map(|n| cards::isbn(&format!("isbn:{n}")))
        .map(|n| cards::isbn13(&n))
        .fold(Vec::new(), |mut all, n| {
            if !all.contains(&n) {
                all.push(n);
            }
            all
        });
    let theirs = match book {
        Book::Isbn(isbn) => isbns.contains(isbn),
        Book::Edition(asked) => *asked == id,
        Book::Work(_) => false,
    };
    anyhow::ensure!(theirs, "Open Library answered for another edition");
    let works: Vec<String> = v["works"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["key"].as_str()?.strip_prefix("/works/"))
        .filter_map(|w| ol_id(w, 'W'))
        .collect();
    let ids = &v["identifiers"];
    Ok(Some(Edition {
        id,
        work: match works.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        },
        isbns,
        ocaid: v["ocaid"]
            .as_str()
            .filter(|o| (1..=100).contains(&o.len()) && slug(o) && !o.starts_with('.'))
            .map(str::to_owned),
        ebook: v["physical_format"].as_str().is_some_and(ebook),
        goodreads: single(&ids["goodreads"]).filter(|g| digits(g)),
        librarything: single(&ids["librarything"]).filter(|l| digits(l)),
        google: single(&ids["google"]).filter(|g| slug(g) && g.len() <= 40),
    }))
}

/// The ISBN-10 of a 978 ISBN-13, Amazon's product ID for a printed book.
pub fn isbn10(isbn13: &str) -> Option<String> {
    let body = isbn13.strip_prefix("978")?.get(..9)?;
    if isbn13.len() != 13 || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sum: u32 = body
        .bytes()
        .enumerate()
        .map(|(i, b)| u32::from(b - b'0') * (10 - i as u32))
        .sum();
    let check = (11 - sum % 11) % 11;
    Some(format!(
        "{body}{}",
        if check == 10 {
            "X".into()
        } else {
            check.to_string()
        }
    ))
}

/// The keys, links and facts (its ISBN-13s) of an edition. `isbn` is the ISBN the link named,
/// if any. The edition's links are keyed by the edition, its work's by the work.
pub fn found(edition: &Edition, isbn: Option<&str>) -> Found {
    let mut found = Found {
        facts: edition
            .isbns
            .iter()
            .map(|n| Fact::new("isbn", n.as_str()))
            .collect(),
        ..Default::default()
    };
    let key = format!("openlibrary:edition:{}", edition.id);
    found.keys.push(Key {
        key: key.clone(),
        via: "openlibrary".into(),
    });
    let link = |uri: String, site: &str| Link::new(uri, site, "openlibrary").keyed(&key);
    found.links.push(link(
        format!("{OPEN_LIBRARY}/books/{}", edition.id),
        "Open Library",
    ));
    if let Some(work) = &edition.work {
        found.keys.push(Key {
            key: format!("openlibrary:work:{work}"),
            via: "openlibrary".into(),
        });
        let mut all = Link::new(
            format!("{OPEN_LIBRARY}/works/{work}"),
            "Open Library",
            "openlibrary",
        )
        .keyed(format!("openlibrary:work:{work}"));
        all.kind = Some("work".into());
        found.links.push(all);
    }
    if let Some(ocaid) = &edition.ocaid {
        found.links.push(link(
            format!("https://archive.org/details/{ocaid}"),
            "Internet Archive",
        ));
    }
    if let Some(id) = &edition.goodreads {
        found.links.push(link(
            format!("https://www.goodreads.com/book/show/{id}"),
            "Goodreads",
        ));
    }
    if let Some(id) = &edition.librarything {
        // LibraryThing's IDs are its works'.
        let mut work = Link::new(
            format!("https://www.librarything.com/work/{id}"),
            "LibraryThing",
            "openlibrary",
        );
        if let Some(w) = &edition.work {
            work = work.keyed(format!("openlibrary:work:{w}"));
        }
        work.kind = Some("work".into());
        found.links.push(work);
    }
    if let Some(id) = &edition.google {
        found.links.push(link(
            format!("https://books.google.com/books?id={id}"),
            "Google Books",
        ));
    }
    // A printed book's ISBN-10 is its Amazon product ID; an ebook's product has its own ID.
    if let Some(ten) = isbn.filter(|_| !edition.ebook).and_then(isbn10) {
        found.links.push(
            Link::new(format!("https://www.amazon.com/dp/{ten}"), "Amazon", "isbn").keyed(&key),
        );
    }
    found
}

/// iTunes' ebook by ISBN in the operator's storefront (`[also_on] apple_storefront`), asked
/// only for an ebook edition's ISBN (a printed book's ISBN finds loose matches, such as another
/// language's edition).
pub fn itunes_request(isbn: &str, storefront: &str) -> String {
    format!("https://itunes.apple.com/lookup?isbn={isbn}&country={storefront}")
}
/// The Apple Books page of the one ebook iTunes lists for the ISBN.
pub fn parse_itunes(status: u16, raw: &[u8], storefront: &str) -> Result<Option<Link>> {
    let Some(v) = media::json(status, raw, &[404])? else {
        return Ok(None);
    };
    let results = v["results"].as_array().context("no results")?;
    Ok(match results.as_slice() {
        [one] if one["kind"] == "ebook" => one["trackId"].as_u64().filter(|id| *id > 0).map(|id| {
            Link::new(
                format!("https://books.apple.com/{storefront}/book/id{id}"),
                "Apple Books",
                "itunes",
            )
        }),
        _ => None,
    })
}

/// The work's other pages, from its Wikidata item (found by its Open Library work ID).
pub const WIKIDATA: &[Property] = &[
    property(
        "P1085",
        "LibraryThing",
        "https://www.librarything.com/work/$1",
        digits,
    ),
    property(
        "P8383",
        "Goodreads",
        "https://www.goodreads.com/work/editions/$1",
        digits,
    ),
    property(
        "P675",
        "Google Books",
        "https://books.google.com/books?id=$1",
        slug,
    ),
    property(
        "P2034",
        "Project Gutenberg",
        "https://www.gutenberg.org/ebooks/$1",
        digits,
    ),
    property(
        "P7823",
        "BookBrainz",
        "https://bookbrainz.org/work/$1",
        uuid,
    ),
];

/// The source: Open Library first, then Apple Books and Wikidata for what it names.
pub struct Books;

#[async_trait]
impl Source for Books {
    fn link_type(&self, uri: &str) -> Option<Option<&'static str>> {
        book(uri).map(|_| Some("Book"))
    }
    fn id(&self) -> &'static str {
        "books"
    }
    fn applies(&self, subject: &Subject) -> bool {
        book(&subject.uri).is_some()
    }
    async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
        match book(&subject.uri) {
            Some(Book::Work(id)) => Self::work(cx, &id).await,
            Some(book) => Self::edition(cx, &book).await,
            None => Ok(Found::default()),
        }
    }
}

impl Books {
    fn options() -> FetchOptions {
        FetchOptions {
            accept: Some("application/json".into()),
            ..Default::default()
        }
    }
    /// The work's Wikidata item (the one carrying its Open Library ID): its Q-id as a fact (the
    /// Open Library work key already joins the work's editions) and its pages, keyed by the work.
    async fn wikidata(cx: &Context, work: &str, kind: Option<&str>) -> Result<Option<Found>> {
        let wikidata::Found::One(item) = wikidata::item_with(&cx.egress, "P648", work).await?
        else {
            return Ok(None);
        };
        let mut found = Found::default();
        found.facts.push(Fact::new("wikidata", item.qid.as_str()));
        for l in media::links(&item, WIKIDATA) {
            let mut l = l.keyed(format!("openlibrary:work:{work}"));
            l.kind = kind.map(str::to_owned);
            found.links.push(l);
        }
        Ok(Some(found))
    }
    fn merge(found: &mut Found, more: Found) {
        found.facts.extend(more.facts);
        for l in more.links {
            if !found.links.iter().any(|f| f.uri == l.uri) {
                found.links.push(l);
            }
        }
    }
    /// An edition: Open Library first, then Apple Books and the work's Wikidata item.
    async fn edition(cx: &Context, book: &Book) -> Result<Found> {
        let f = cx
            .egress
            .discover(&edition_request(book), &Self::options(), 3600)
            .await?;
        let Some(edition) = parse_edition(f.status, &f.body, book)? else {
            return Ok(Found::default());
        };
        let isbn = match book {
            Book::Isbn(isbn) => Some(isbn.clone()),
            // An edition page names its ISBN only when the edition has exactly one.
            _ => match edition.isbns.as_slice() {
                [one] => Some(one.clone()),
                _ => None,
            },
        };
        let mut found = found(&edition, isbn.as_deref());
        // A failing secondary service leaves what Open Library said.
        let mut deferred = Deferred::default();
        if let (true, Some(isbn)) = (edition.ebook, &isbn) {
            let itunes = FetchOptions {
                extra_types: &["text/javascript"],
                ..Self::options()
            };
            let storefront = &cx.config.also_on.apple_storefront;
            let apple = async {
                let f = cx
                    .egress
                    .discover(&itunes_request(isbn, storefront), &itunes, 3600)
                    .await?;
                parse_itunes(f.status, &f.body, storefront)
            };
            let apple = deferred.take("itunes", apple.await);
            let key = format!("openlibrary:edition:{}", edition.id);
            found.links.extend(apple.map(|l| l.keyed(key)));
        }
        if let Some(work) = &edition.work {
            // The item is the work's: its pages are the work's too.
            if let Some(more) =
                deferred.take("wikidata", Self::wikidata(cx, work, Some("work")).await)
            {
                Self::merge(&mut found, more);
            }
        }
        deferred.finish(found)
    }
    /// A work: its Open Library record (following merges to the work it is now), then its
    /// Wikidata item.
    async fn work(cx: &Context, asked: &str) -> Result<Found> {
        let mut id = asked.to_owned();
        for _ in 0..3 {
            let f = cx
                .egress
                .discover(&work_request(&id), &Self::options(), 3600)
                .await?;
            match parse_work(f.status, &f.body, &id)? {
                WorkRecord::Work => {
                    let mut found = Found::default();
                    found.keys.push(Key {
                        key: format!("openlibrary:work:{id}"),
                        via: "openlibrary".into(),
                    });
                    if id != asked {
                        found.links.push(
                            Link::new(
                                format!("{OPEN_LIBRARY}/works/{id}"),
                                "Open Library",
                                "openlibrary",
                            )
                            .keyed(format!("openlibrary:work:{id}")),
                        );
                    }
                    let mut deferred = Deferred::default();
                    if let Some(more) =
                        deferred.take("wikidata", Self::wikidata(cx, &id, None).await)
                    {
                        Self::merge(&mut found, more);
                    }
                    return deferred.finish(found);
                }
                WorkRecord::Redirect(to) => id = to,
                WorkRecord::Gone => break,
            }
        }
        Ok(Found::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::also_on::media::fixture;

    /// Its links are typed by what they are, whatever their pages call themselves
    /// (`media::link_type`); a page under an object is not typed.
    #[test]
    fn its_links_are_typed_by_what_they_are() {
        for (uri, kind) in [
            (
                "https://openlibrary.org/works/OL27482W/The_Hobbit",
                Some("Book"),
            ),
            ("https://openlibrary.org/books/OL7353617M", Some("Book")),
        ] {
            assert_eq!(super::super::media::link_type(uri), kind, "{uri}");
        }
    }

    fn read(name: &str) -> Vec<u8> {
        fixture::read("books", name)
    }
    fn edition(name: &str, isbn: &str) -> Edition {
        parse_edition(200, &read(name), &Book::Isbn(isbn.into()))
            .unwrap()
            .unwrap()
    }
    fn sites(found: &Found) -> Vec<(&str, &str)> {
        found
            .links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect()
    }

    #[test]
    fn links_that_name_a_book() {
        let gatsby = Book::Isbn("9780743273565".into());
        for uri in [
            "isbn:9780743273565",
            "isbn:0743273567",
            "isbn:978-0-7432-7356-5",
            "https://openlibrary.org/isbn/9780743273565",
            "https://openlibrary.org/isbn/0743273567.json",
        ] {
            assert_eq!(book(uri), Some(gatsby.clone()), "{uri}");
        }
        assert_eq!(
            book("https://openlibrary.org/books/OL23109860M/The_Great_Gatsby"),
            Some(Book::Edition("OL23109860M".into()))
        );
        for uri in [
            "isbn:9780743273566",
            "https://openlibrary.org/books/OL23109860W",
            "https://openlibrary.org:8080/isbn/9780743273565",
            "https://example.org/isbn/9780743273565",
            "https://www.goodreads.com/book/isbn/9780743273565",
        ] {
            assert_eq!(book(uri), None, "{uri}");
        }
        assert_eq!(
            edition_request(&gatsby),
            "https://openlibrary.org/isbn/9780743273565.json"
        );
    }

    #[test]
    fn a_printed_edition() {
        let e = edition("ol-isbn-9780743273565.json", "9780743273565");
        assert_eq!(e.id, "OL23109860M");
        assert_eq!(e.work.as_deref(), Some("OL468431W"));
        assert!(!e.ebook);
        let found = found(&e, Some("9780743273565"));
        let keys: Vec<&str> = found.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "openlibrary:edition:OL23109860M",
                "openlibrary:work:OL468431W"
            ]
        );
        assert_eq!(
            sites(&found),
            [
                ("Open Library", "https://openlibrary.org/books/OL23109860M"),
                ("Open Library", "https://openlibrary.org/works/OL468431W"),
                (
                    "Internet Archive",
                    "https://archive.org/details/greatgat00fitz"
                ),
                ("Goodreads", "https://www.goodreads.com/book/show/4671"),
                ("LibraryThing", "https://www.librarything.com/work/2964"),
                (
                    "Google Books",
                    "https://books.google.com/books?id=iXn5U2IzVH0C"
                ),
                ("Amazon", "https://www.amazon.com/dp/0743273567"),
            ]
        );
        assert_eq!(found.links[1].kind.as_deref(), Some("work"));
        assert_eq!(found.facts, [Fact::new("isbn", "9780743273565")]);
        assert!(found
            .links
            .iter()
            .all(|l| l.kind.as_deref() != Some("read")));
    }

    #[test]
    fn an_ebook_edition() {
        let e = edition("ol-isbn-9780804139038.json", "9780804139038");
        assert!(e.ebook, "Open Library says Ebook");
        let found = found(&e, Some("9780804139038"));
        assert!(
            !found.links.iter().any(|l| l.site == "Amazon"),
            "an ebook is not its ISBN-10 on Amazon"
        );
        assert_eq!(
            itunes_request("9780804139038", "us"),
            "https://itunes.apple.com/lookup?isbn=9780804139038&country=us"
        );
        let apple = parse_itunes(200, &read("itunes-isbn-9780804139038.json"), "us")
            .unwrap()
            .unwrap();
        assert_eq!(
            (apple.site.as_str(), apple.uri.as_str()),
            ("Apple Books", "https://books.apple.com/us/book/id721429965")
        );
        // An ebook ISBN iTunes doesn't sell.
        assert_eq!(
            parse_itunes(200, &read("itunes-isbn-9780141913414.json"), "us").unwrap(),
            None
        );
        assert!(
            edition("ol-isbn-9780141913414.json", "9780141913414").ebook,
            "Epub"
        );
    }

    #[test]
    fn a_printed_isbn_is_never_asked_of_itunes() {
        // iTunes answers The Great Gatsby's printed ISBN with a Korean edition: a loose match,
        // which is why only an ebook edition's ISBN is asked.
        let raw = read("itunes-isbn-9780743273565.json");
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["results"][0]["kind"], "ebook");
        assert!(!v["results"][0]["trackName"]
            .as_str()
            .unwrap()
            .contains("Great Gatsby"));
        assert!(!edition("ol-isbn-9780743273565.json", "9780743273565").ebook);
    }

    #[test]
    fn the_answer_must_be_the_edition_asked() {
        // ISBN 9780140328721 is Fantastic Mr. Fox, whatever a caller believes.
        let e = edition("ol-isbn-9780140328721.json", "9780140328721");
        assert_eq!(
            (e.id.as_str(), e.work.as_deref()),
            ("OL7353617M", Some("OL45804W"))
        );
        assert!(parse_edition(
            200,
            &read("ol-isbn-9780140328721.json"),
            &Book::Isbn("9780743273565".into())
        )
        .is_err());
        assert!(parse_edition(
            200,
            &read("ol-isbn-9780140328721.json"),
            &Book::Edition("OL7353617M".into())
        )
        .unwrap()
        .is_some());
        assert_eq!(
            parse_edition(
                404,
                &read("ol-isbn-missing.json"),
                &Book::Isbn("9798999999993".into())
            )
            .unwrap(),
            None
        );
        assert!(parse_edition(429, b"", &Book::Isbn("9780743273565".into())).is_err());
    }

    #[test]
    fn work_links_and_records() {
        assert_eq!(
            book("https://openlibrary.org/works/OL27482W/The_Hobbit"),
            Some(Book::Work("OL27482W".into()))
        );
        assert_eq!(
            book("https://openlibrary.org/works/OL27482W.json"),
            Some(Book::Work("OL27482W".into()))
        );
        assert_eq!(book("https://openlibrary.org/works/OL27482M"), None);
        assert_eq!(book("https://user:pw@openlibrary.org/works/OL27482W"), None);
        assert_eq!(
            work_request("OL27482W"),
            "https://openlibrary.org/works/OL27482W.json"
        );
        assert_eq!(
            parse_work(200, &read("ol-work-OL27482W.json"), "OL27482W").unwrap(),
            WorkRecord::Work
        );
        assert!(parse_work(200, &read("ol-work-OL27482W.json"), "OL893414W").is_err());
        // Wikidata's Dune item names OL893415W, which Open Library merged into OL893414W.
        assert_eq!(
            parse_work(200, &read("ol-work-OL893415W.json"), "OL893415W").unwrap(),
            WorkRecord::Redirect("OL893414W".into())
        );
        assert_eq!(parse_work(404, b"{}", "OL1W").unwrap(), WorkRecord::Gone);
    }

    #[test]
    fn isbn_10_for_amazon() {
        assert_eq!(isbn10("9780743273565").as_deref(), Some("0743273567"));
        assert_eq!(isbn10("9780140328721").as_deref(), Some("0140328726"));
        assert_eq!(isbn10("9780804429573").as_deref(), Some("080442957X"));
        assert_eq!(isbn10("9798999999993"), None, "979 ISBNs have no ISBN-10");
    }

    #[test]
    fn the_works_wikidata_item() {
        let item = fixture::item("Q214371");
        // The item lists two Open Library works; the one asked is among them.
        assert!(item.id("P648").contains(&"OL468431W".to_string()));
        let links = media::links(&item, WIKIDATA);
        let got: Vec<(&str, &str)> = links
            .iter()
            .map(|l| (l.site.as_str(), l.uri.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("LibraryThing", "https://www.librarything.com/work/2964"),
                (
                    "Goodreads",
                    "https://www.goodreads.com/work/editions/245494"
                ),
                (
                    "Project Gutenberg",
                    "https://www.gutenberg.org/ebooks/64317"
                ),
            ]
        );
    }
}
