# Sources: cards from official APIs

Some services answer exactly from their own API where their page is walled, thin or has moved.
For their links, Pinakes reads that API instead of the page. Every source here:

- reads one object by an identifier taken from the link (never a search by title or name);
- needs no key;
- makes every address of the same object one resource: the other addresses are
  [aliases](../api.md#aliases) of one root;
- records itself on the card: `details.from` names the source and `details.facts` holds the
  fields its API names for the object, by name (lists joined with `; `).

A few sources are page hooks instead (WordPress, Ghost): they claim no links. The web route
fetches the page as usual, and when the page names its own record on its service (a header or
head tag on the page's own origin), the source reads that record and improves the page's card,
within the web fetch's deadline and at most 3 seconds. The card then names the source in
`details.from` too; a failure leaves the page's own card.

One source is a wall hook ([`feed`](feed.md)): when a page answers with an access wall, it looks
for that exact link in the same site's RSS or Atom feed or news sitemap, and the item makes the
card.

## Switches

Each source has its own switch, on by default:

```toml
[sources.federal_register]
enabled = false
```

(or `PINAKES__SOURCES__FEDERAL_REGISTER__ENABLED=false`). A switched-off source's web links get
ordinary web cards; a link in its own identifier scheme (such as `fr:…`) waits until the source is
on again. The whole route can be switched off with `[cards.official] enabled = false`.

## Politeness

Every request carries the operator's contact in the User-Agent (`http.contact`, see
[operations](../operations.md#deployment-and-egress)); while it is empty, the links of a source
whose service requires an email address ([SEC EDGAR](sec.md)) wait and are retried hourly.
Requests share the per-host budget (`limits.host_requests_per_second`, one per second by
default) and never have more than one request in flight per host. A source whose service
publishes a lower limit lowers that host's rate (a windowed limit as its average rate); a host
whose next request is further away than a fetch may wait is retried later. `limits.host_rates`
overrides both. Each page below names the service's limits, licence and any attribution the card
carries.

After an upgrade that adds a source, run `pinakesd reextract web official`, so links that failed
or got plain web cards before are read by their source.

## Sources

- [`artic`](artic.md): Art Institute of Chicago artworks by id.
- [`bluesky`](bluesky.md): Bluesky posts and profiles.
- [`cleveland`](cleveland.md): Cleveland Museum of Art artworks by accession number.
- [`commons`](commons.md): Wikimedia Commons files (images, video, audio) by file name.
- [`courtlistener`](courtlistener.md): US court opinions by cluster id.
- [`discourse`](discourse.md): Topics and posts on any Discourse forum.
- [`ecfr`](ecfr.md): sections and parts of the Code of Federal Regulations.
- [`eur_lex`](eur_lex.md): EU law by CELEX number, ELI or Official Journal id.
- [`federal_register`](federal_register.md): Federal Register documents by document number.
- [`fedlex`](fedlex.md): Swiss federal law in the classified compilation, by ELI.
- [`feed`](feed.md): wall hook: a walled page's card from the exact item in its site's RSS/Atom feed or news sitemap
- [`figshare`](figshare.md): Figshare articles (datasets, figures, papers) by article id.
- [`forgejo`](forgejo.md): repositories, branches, tags, releases and files on Forgejo and Gitea instances (Codeberg, gitea.com, others)
- [`ghost`](ghost.md): page hook: a Ghost post's title, excerpt, image, dates, authors and tags from the Content API
- [`gitlab`](gitlab.md): projects, branches, tags, releases and files on gitlab.com and other GitLab instances
- [`gleif`](gleif.md): Legal Entity Identifier records.
- [`govinfo`](govinfo.md): US public laws, bills, US Code sections and the Statutes at Large.
- [`hacker_news`](hacker_news.md): Hacker News items by id (and hnsignals.com pages).
- [`huggingface`](huggingface.md): Hugging Face models, datasets and Spaces by repo id.
- [`internet_archive`](internet_archive.md): Internet Archive items (books, films, audio, software…) by identifier.
- [`legislation_gov_uk`](legislation_gov_uk.md): UK Acts, statutory instruments and retained EU law.
- [`lobsters`](lobsters.md): Lobsters stories by short id.
- [`mastodon`](mastodon.md): Mastodon posts on any server.
- [`mempool`](mempool.md): Bitcoin blocks, transactions and addresses, and Lightning nodes and
  channels, from mempool.space (Esplora as fallback).
- [`met`](met.md): The Metropolitan Museum of Art's works by object id.
- [`public_inbox`](public_inbox.md): mailing-list messages in public-inbox archives (gnusha.org, lore.kernel.org, others), and old bitcoin-dev pipermail addresses
- [`sec`](sec.md): SEC EDGAR filings, their documents and companies.
- [`stacker_news`](stacker_news.md): Stacker News posts and comments by item id.
- [`swiss_parliament`](swiss_parliament.md): Swiss Parliament business items (Curia Vista) by affair id.
- [`uk_parliament_bills`](uk_parliament_bills.md): UK Parliament bills and Acts by bill id.
- [`wordpress`](wordpress.md): page hook: a WordPress post's editorial title, excerpt, dates and featured image from its REST record
- [`zefix`](zefix.md): Swiss companies in the commercial register, by register id.
- [`zenodo`](zenodo.md): Zenodo records (datasets, software, papers) by record id.
