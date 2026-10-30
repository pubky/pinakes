# `books`: Open Library, Amazon, Apple Books and Wikidata for a book

An [also on](../api.md#also-on) source.

`books`: an ISBN (`isbn:…`, `openlibrary.org/isbn/{isbn}`), an Open Library edition
page (`openlibrary.org/books/OL…M`) or work page (`openlibrary.org/works/OL…W`) is looked up
exactly, never by title:
- Open Library's edition record must list the ISBN asked. Its edition and work IDs are keys
  (`openlibrary:edition:…`, `openlibrary:work:…`, so the editions of one work join, a hint like
  every group; `key=` takes either as it is recorded), its ISBN-13s are `isbn` facts, and it links the edition, its work
  (`kind: "work"`), its Internet Archive copy (`ocaid`), and its Goodreads edition, LibraryThing
  work (`kind: "work"`) and Google Books pages when it holds exactly one ID for each.
- A work page's key is `openlibrary:work:…`. Open Library's record must be that work; an ID it
  merged into another work is followed to that work, which it then links.
- A printed edition's 978 ISBN gives its Amazon page (`amazon.com/dp/{ISBN-10}`, `via: "isbn"`).
- An ebook edition's ISBN gives its Apple Books page when iTunes lists exactly one ebook for it.
  iTunes' answer carries no ISBN, so Open Library's format field ("Ebook", "Epub", …) is the
  guard. A printed edition's ISBN is never asked: iTunes answers it with loose matches, such as
  an edition in another language.
- The work's Wikidata item (the one carrying its Open Library work ID) gives its Q-id as the
  `wikidata` fact and its LibraryThing, Goodreads (the work's editions), Google Books, Project
  Gutenberg and BookBrainz pages, each with `kind: "work"` on an edition's card. A book's
  details can therefore list two Goodreads pages: its edition and its work's editions.
- A book's page on another service is a starting point too: a Goodreads book
  (`goodreads.com/book/show/{id}`, an edition, Wikidata's P2969) and any page the work's item
  lists (Goodreads' `work/editions/{id}`, LibraryThing, Google Books, Project Gutenberg,
  BookBrainz). Such a page is on its service: its card is named after it (`site`: `Goodreads`
  for a book and for a work's editions alike), as its links are, whatever the page calls
  itself. Each finds the one Wikidata item carrying its value (read back and compared
  exactly; an ID two items carry is ambiguous and gives nothing). The item's Open Library work or
  edition ID (P648) is then looked up as above, or, for an edition's item without one, the ID of
  the work it is an edition of (P629). The Open Library page is a link, and its key joins the
  page with the work's or edition's other links.
- A failing iTunes or Wikidata request leaves what Open Library said.
- Each link carries the key it was found through: the edition's pages (Amazon and Apple Books
  too) its edition key, the work's pages its work key. A tagged Goodreads or LibraryThing link
  therefore joins its edition or work.

Goodreads' `book/isbn/{isbn}` redirect is never used: it answers an unknown ISBN with an
unrelated book.

## Operations

Each book costs one Open Library request, one iTunes lookup for an ebook edition, and two
Wikidata requests when Open Library names its work.
