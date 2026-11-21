# `games`: a game's other stores

A lookup family of [also on](../api.md#also-on) (`lookups/games.rs`).

- `steam`: a Steam game's own page (store, community hub, SteamDB, ProtonDB or PCGamingWiki appid
  link; never a page under it: a discussion, review, guide, workshop item or news item) gets its
  Steam store and SteamDB pages, its ProtonDB page when ProtonDB has reports, its PCGamingWiki page where the appid
  lookup leads, and its GOG page through GOG GamesDB (only GOG's and Steam's own rows), with
  `steam:` and `gog:` keys.
- `wikidata_stores`: a game's own page on Steam or GOG (never a page under it), or on any store
  or game database the item lists below (Epic Games Store `store.epicgames.com/p/{slug}`, IGDB,
  MobyGames, PCGamingWiki, HowLongToBeat, by the address the item's own link has), gets the one
  Wikidata item carrying that page's ID (read back and compared exactly; an item its successor
  replaces gives way to it; several others are ambiguous and give nothing). It gets the item's
  other stores and game databases (Epic Games Store, itch.io, IGDB, MobyGames, PCGamingWiki,
  HowLongToBeat) with one value each as links. Only when the item carries no other ID on the
  page's store is the page the item's: then its Q-id is a key and, off Steam, its one Steam
  appid is the `steam:` key, so the page joins the game's Steam page. An item with two values
  on one store (two Steam appids; a game and its expansion pass on GOG) gives either page links
  only, no key. An app's store pages are the
  `apps` family's, when the build has it.


A card on these hosts is named after its service (`site`: Steam, SteamDB, ProtonDB,
PCGamingWiki, GOG) only when it is such an
object's own page. On PCGamingWiki that is an article, `/wiki/{title}` outside a namespace
(`Special:`, `Category:`, `User_talk:`, …) and not the main page, with no query but one that
marks where the link was shared from (`utm_*`, `ref`); `?action=history` is another page.

## Keys

`steam:{appid}` and `gog:{id}` (`steam`): each names one object, so a [group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds the links that carry it.

## Operations

Each Steam game costs up to four requests (ProtonDB, PCGamingWiki, GOG GamesDB, GOG), each store
link two Wikidata requests. When ProtonDB, PCGamingWiki or GOG fails, the game's own key is
recorded and the run comes back after a failed run's backoff
([operations](../operations.md#also-on)).
