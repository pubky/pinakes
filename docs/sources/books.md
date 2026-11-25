# `books`: Open Library, Amazon, Apple Books and Wikidata for a book

This source runs on links to books. It looks the book up by its exact ID, never by title, and adds
Open Library keys, the book's ISBNs and its pages on other services as
[also on](../api.md#also-on) links. No key is needed.

| | |
| --- | --- |
| Source | `books`, also-on source |
| Runs on | `isbn:` links, Open Library edition and work pages, and a book's page on Goodreads, LibraryThing, Google Books, Project Gutenberg or BookBrainz ([links](#links-it-reads)) |
| Asks | Open Library (`GET https://openlibrary.org/isbn/{isbn}.json`, `/books/{OL…M}.json`, `/works/{OL…W}.json`); iTunes for an ebook edition (`GET https://itunes.apple.com/lookup?isbn={isbn}&country={storefront}`); Wikidata (`https://www.wikidata.org/w/api.php`) |
| Credentials | None |
| Rate | At most 3 requests per second on `openlibrary.org`, 200 a minute on `www.wikidata.org` and about 20 a minute on `itunes.apple.com` (the services' published limits); the shared default of 1 per second (`limits.host_requests_per_second`) applies while it is lower. Every source that asks these hosts shares the rate |
| Rows live | 30 days; rows expire after 60 |
| Keys | `openlibrary:edition:{OL…M}`, `openlibrary:work:{OL…W}` |
| Links | The edition's and the work's pages on Open Library, Internet Archive, Goodreads, LibraryThing, Google Books, Amazon, Apple Books, Project Gutenberg and BookBrainz, exact |
| Facts | `isbn`, `wikidata` |
| Switch | `[also_on.sources.books] enabled`, on by default |

## What it does

### Links it reads

| Link | What it names |
| --- | --- |
| `isbn:{ISBN-10 or ISBN-13}`, `https://openlibrary.org/isbn/{isbn}` | The edition with that ISBN |
| `https://openlibrary.org/books/{OL…M}` | That edition |
| `https://openlibrary.org/works/{OL…W}` | That work |
| `https://www.goodreads.com/book/show/{id}` | An edition (Wikidata property P2969) |
| `https://www.goodreads.com/work/editions/{id}` | A work (P8383) |
| `https://www.librarything.com/work/{id}` | A work (P1085) |
| `https://books.google.com/books?id={id}` | A book (P675) |
| `https://www.gutenberg.org/ebooks/{id}` | A book (P2034) |
| `https://bookbrainz.org/work/{uuid}` | A work (P7823) |

Each form is read with or without `www.`. Open Library links can also end in `.json`, and the
edition and work pages and `book/show` links can carry the title. An ISBN must have a valid check
digit. An ISBN-10 is looked up as its ISBN-13.

Every resolved card of these links has the type `Book`. A card of a Goodreads, LibraryThing, Google
Books, Project Gutenberg or BookBrainz page also takes that service's name as its `site`, whatever
the page calls itself. `site` is "Goodreads" for a book and for a work's editions alike. Goodreads'
`book/isbn/{isbn}` links are never read or used: that redirect answers an unknown ISBN with an
unrelated book.

### An edition

1. Open Library's record must be the edition asked. It must list the link's ISBN, or have the
   link's edition ID. Any other record is an error, and the run is retried.
2. The record gives the edition's key, its ISBN-13s and its pages. When it names exactly one work,
   it also gives the work's key and page.
3. For an ebook edition, the source asks iTunes for the edition's ISBN. The Apple Books page counts
   only when iTunes lists exactly one result and that result is an ebook.
4. When the record names a work, the source reads the work's Wikidata item: the one item that
   carries the Open Library work ID (P648).

An edition is an ebook when Open Library's format field says so ("Ebook", "Epub", "Electronic
resource", "Kindle Edition" and the like). The format is the guard because iTunes' answer carries
no ISBN to compare, and iTunes answers a printed ISBN with loose matches such as an edition in
another language. An edition of any other format, or of none, is never asked of iTunes.

The ISBN used for iTunes and Amazon is the one the link names. An edition page has one only when
the edition lists exactly one ISBN-13.

### A work

Open Library's record must be that work. When the ID was merged into another work, the source
follows it (it reads at most three records) and links the work it ends on. An unknown or deleted
work gives nothing. The source then reads the work's Wikidata item as for an edition.

### A book's page on another service

1. The source finds the one Wikidata item that carries the link's value. The search index only
   proposes: each hit is read back and its value compared exactly. An ID that two items carry is
   ambiguous and gives nothing.
2. The item must name exactly one Open Library ID (P648), of a work or of an edition. When it
   names none, or several, and is an edition of exactly one work (P629), the source reads that
   work's item for the ID.
3. That work or edition is then looked up as above. The Open Library page is a link, and the
   page's links to its own service are left out.

## Keys, links and facts

Both key namespaces name one object each, so a group holds at most one edition and one work.
Editions and translations of one work never join: each edition keeps its own
[group](../api.md#groups), and the work page joins at most one of them. A search takes either key
as it is recorded, for example `key=openlibrary:work:OL468431W`.

All links are exact. Each one carries the key it was found through, so a tagged Goodreads or
LibraryThing link joins its edition or work.

| Link | From | Key | When |
| --- | --- | --- | --- |
| Open Library edition | Open Library | Edition | Always, for an edition |
| Open Library work | Open Library | Work | The edition names exactly one work, or the work's ID was merged |
| Internet Archive (`archive.org/details/{ocaid}`) | Open Library | Edition | The edition has an `ocaid` |
| Goodreads (`book/show/{id}`) | Open Library | Edition | The edition lists exactly one ID |
| LibraryThing (`work/{id}`) | Open Library | Work, if any | The edition lists exactly one ID |
| Google Books | Open Library | Edition | The edition lists exactly one ID |
| Amazon (`amazon.com/dp/{ISBN-10}`, `via: "isbn"`) | The ISBN | Edition | A 978 ISBN of an edition that is no ebook |
| Apple Books | iTunes | Edition | An ebook edition, as above |
| LibraryThing, Goodreads (`work/editions/{id}`), Google Books, Project Gutenberg, BookBrainz | Wikidata | Work | The work's item has exactly one valid value for the property |

On an edition's card, the links to its work and its work's pages have `kind: "work"`: the Open
Library work, LibraryThing and every page from the work's Wikidata item. Such a card can list two
Goodreads pages, its own edition's and its work's editions.

| Fact | Meaning |
| --- | --- |
| `isbn` | Each ISBN-13 of the edition, as Open Library lists them |
| `wikidata` | The Q-id of the work's Wikidata item |

The Q-id is a fact only: the Open Library work key already joins the work's links.

## Cost

| Link | Requests |
| --- | --- |
| An edition | One to Open Library; one iTunes lookup for an ebook; two to Wikidata (search and read) when the edition names one work |
| A work | One to Open Library (up to three for a merged ID); two to Wikidata |
| Another service's page | Two to Wikidata, a third when the item is an edition without a single Open Library ID; then as for the work or edition |

Every answer is cached for `limits.discovery_cache_secs` (an hour by default) and shared with the
other sources that ask the same. A failed iTunes request, or a failed Wikidata request for the
work's item, leaves what Open Library said: the source adds that, deletes nothing it recorded
before, and the run comes back for the rest. A failed Open Library request, or a failed search for
another service's page, fails the run, which is retried while the stored rows stay.

The Apple Books lookup uses the storefront `[also_on] apple_storefront` (`us` by default).

## Turn it off

```toml
[also_on.sources.books]
enabled = false
```

Its stored keys, links and facts stop showing at the next start, and its groups re-form without
them. The cards keep their `Book` type and their `site`, which come from the link alone.
