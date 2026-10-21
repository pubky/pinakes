# `games`: a game's and an app's other stores

A lookup family of [also on](../api.md#also-on) (`lookups/games.rs`).

- `steam`: a Steam game's own page (store, community hub, SteamDB, ProtonDB or PCGamingWiki appid
  link; never a page under it: a discussion, review, guide, workshop item or news item) gets its
  Steam store and SteamDB pages, its ProtonDB page when ProtonDB has reports, its PCGamingWiki page where the appid
  lookup leads, and its GOG page through GOG GamesDB (only GOG's and Steam's own rows), with
  `steam:` and `gog:` keys.
- `wikidata_stores`: an app's or game's own page on Steam, GOG, the App Store, Google Play,
  F-Droid, Flathub or the Snap Store (never a page under it) gets the one Wikidata item carrying that store's ID (read back and compared exactly;
  several are ambiguous and give nothing), its other stores and game databases (Epic Games
  Store, itch.io, IGDB, MobyGames, PCGamingWiki, HowLongToBeat, Homebrew) with one value each,
  and its Q-id as key when the item carries no other ID on that store (an item with two Steam
  appids is neither page's).


A card on these hosts is named after its service (`site`: Steam, SteamDB, ProtonDB,
PCGamingWiki, GOG, App Store, Google Play, F-Droid, Flathub, Snap Store) only when it is such an
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
