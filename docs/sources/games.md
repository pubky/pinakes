# `games`: a game's other stores

This lookup family of [also on](../api.md#also-on) runs on a game's own page on a store or in a game
database. It adds the game's pages elsewhere, from Steam's own ID or through Wikidata.

| | |
| --- | --- |
| Source | `games`, lookup family with sources `steam`, `wikidata_stores` |
| Runs on | `steam`: a Steam game's own page. `wikidata_stores`: a game's own page on a store or database in the table below |
| Asks | `steam`: ProtonDB, PCGamingWiki, GOG GamesDB and GOG. `wikidata_stores`: `https://www.wikidata.org/w/api.php` |
| Credentials | None |
| Rate | At most 200 requests per minute on `www.wikidata.org`; the shared default of 1 per second applies while it is lower. The other hosts: shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Rows live | 30 days; rows expire after twice that |
| Keys | `steam:{appid}`, `gog:{id}`, `wikidata:{Q-id}` |
| Links | Steam, SteamDB, ProtonDB, PCGamingWiki, GOG, Epic Games Store, itch.io, IGDB, MobyGames, HowLongToBeat. All are exact, and none carries a key |
| Facts | None |
| Switch | `[also_on.sources.<id>] enabled` for each of the two sources, on by default |

A page under a game is another object and is never read: a discussion, review, guide, workshop item
or news item.

## What it does

### `steam`

The source reads a game's own page in these forms, with a positive appid of up to ten digits:

| Site | Link |
| --- | --- |
| Steam store | `store.steampowered.com/app/{appid}[/{name}]` |
| Steam Community | `steamcommunity.com/app/{appid}` |
| SteamDB | `steamdb.info/app/{appid}` |
| ProtonDB | `protondb.com/app/{appid}` |
| PCGamingWiki | `pcgamingwiki.com/api/appid.php?appid={appid}` |

1. The card gets the key `steam:{appid}` and the Steam store and SteamDB pages. These come from the
   appid alone, whatever the other services answer. SteamDB is never asked.
2. ProtonDB: the card gets the game's page when ProtonDB has a summary with a tier for it.
3. PCGamingWiki: the source asks the appid lookup and does not follow its redirect. The card gets the
   `https://www.pcgamingwiki.com/wiki/…` page the redirect names.
4. GOG: GOG GamesDB's record for the Steam game must list the appid among its Steam rows and exactly
   one numeric GOG row. Other platforms' rows are ignored. Then the card gets the key `gog:{id}`, and
   GOG's product record gives the store page. That page is a link only when GOG calls the product a
   game and does not mark it secret.

### `wikidata_stores`

The source reads a game's own page in these forms:

| Site | Link | Wikidata property |
| --- | --- | --- |
| Steam | `store.steampowered.com/app/{appid}[/{name}]` | P1733 |
| GOG | `gog.com/[{lang}/]game/{slug}`, `gog.com/movie/{slug}` | P2725 |
| Epic Games Store | `store.epicgames.com/p/{slug}` | P6278 |
| IGDB | `igdb.com/games/{slug}` | P5794 |
| MobyGames | `mobygames.com/game/{id}` | P11688 |
| PCGamingWiki | `pcgamingwiki.com/wiki/{title}` | P6337 |
| HowLongToBeat | `howlongtobeat.com/game/{id}` | P2816 |

1. The source searches Wikidata for items that carry the page's ID. An ID with characters other than
   ASCII letters, digits and `._-` (plus the `/` in a GOG ID) is not searched, so a PCGamingWiki
   title with a `:` is not.
2. It reads the items back and compares the ID byte for byte. Of several items, one that another of
   them replaces (P1366 "replaced by" or P1365 "replaces") gives way to its successor. If several still
   carry the ID, or the search has more than five hits, the source records nothing.
3. The card gets the item's other stores and databases as links. A property with several valid values
   adds no link.
4. When the item carries no other ID on the page's site, the page is the item's: its Q-id is the key
   `wikidata:{Q-id}`. Off Steam, the item's one Steam appid is also the key `steam:{appid}`, so the page
   joins the game's Steam page.

An item with two IDs on one site, such as two Steam appids or a game and its expansion pass on GOG,
gives either page links only. Pages of apps are read by the `apps` family.

## Keys, links and facts

| Key | Source | Recorded when |
| --- | --- | --- |
| `steam:{appid}` | `steam` | Always |
| `steam:{appid}` | `wikidata_stores` | Off Steam: the page is the item's, and the item names one Steam appid |
| `gog:{id}` | `steam` | GOG GamesDB names one GOG product for the game |
| `wikidata:{Q-id}` | `wikidata_stores` | The page is the item's |

`steam:` and `gog:` each name one object, so a [group](../api.md#groups) holds at most one value of
each, and [`key=`](../api.md#search) finds the links that carry them.

Every link's `via` names the service or hub it came from: `steam`, `protondb`, `pcgamingwiki`, `gog`
or `wikidata`. itch.io has no page form of its own here, so an itch.io page is a link and never a
starting point.

A card on Steam, SteamDB, ProtonDB, PCGamingWiki or GOG is named after its service (`site`) only on a
game's own page. On PCGamingWiki that is an article: `/wiki/{title}` outside a namespace (`Special:`,
`Category:`, `User_talk:`, …), not the main page, and without a query other than a share marker
(`utm_*`, `ref` and the like). `?action=history` is another page.

## Cost

| Source | Requests per card |
| --- | --- |
| `steam` | Up to four: ProtonDB, PCGamingWiki, GOG GamesDB, and GOG when GamesDB names a product |
| `wikidata_stores` | One search and one read of the items it finds. When the search finds several items, one more read |

Answers are cached for `limits.discovery_cache_secs` (an hour by default), and Wikidata's are shared
with the other sources that ask for them.

When ProtonDB, PCGamingWiki or GOG fails, the card keeps its own key and what else was found, its stored
rows stay, and the run comes back after a failed run's backoff ([operations](../operations.md#runs)). A
busy Wikidata fails the run of `wikidata_stores`, which is retried. A Wikidata answer over
`limits.hub_body_bytes` gives nothing.

## Turn it off

Switch off each source on its own:

```toml
[also_on.sources.steam]
enabled = false

[also_on.sources.wikidata_stores]
enabled = false
```

After the restart, a source's stored keys and links stop showing, and groups leave its keys out.
They are kept until they expire and show again when the source is on.
