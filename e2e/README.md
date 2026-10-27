# End-to-end checks

The harness builds tags with `pubky-app-specs` and, in live mode, writes them with
`@synonymdev/pubky`. The samples cover an OSM node, geo pin, event page,
ICS feed (the mock exercises recurrence), NIP-52 naddr, DOI with its publisher page, ISBN, oEmbed
video, ordinary web page, one or more links of every source this build has (each official
source's own samples: its `Source::e2e` sample and recorded answers in the mock, and
`e2e/samples/<name>.json` live, where the also-on sources' samples are too, such as music, film,
book and podcast pages whose cards must carry the object's name, not the page's), and live a Pubky object of another app. It checks the
Nexus reference feed, Pinakes typed documents, facets, bbox/Event/venue search,
occurrences, changes, group membership, same-event hints and Nexus hydration by URI (`by_ids` with `uris`: the
cards' own `uri` and `details.aliases`, and an occurrence's `occurrence.feed_uri`, matched by
`details.uri`; Pinakes is read by URI too, its IDs being its own), then deletes
its tags and waits for both indexes to remove their references. `by_ids` lists at most five
tags per resource, so when a shared live resource already carries five other tags the
run's own tag is confirmed through `/v0/resource/{id}/tags` instead, and the MCP `social` tool,
which reads the same batch view, is held to that view's labels.

Requires Node >=20, Rust, and PostgreSQL 16 with PostGIS and h3 extensions installed
as described in the main README. `npm ci --prefix e2e` installs the pinned SDKs.

## Offline fixture flow (CI)

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 scripts/test
npm ci --prefix e2e
npm test --prefix e2e
CARGO_PROFILE_DEV_DEBUG=0 scripts/e2e --mock
```

Mock mode runs the same JavaScript assertions and real tag builder. A local Rust
stack runs the actual schema, ingestion, resolver queue, parsers, storage,
groups and API; only homeserver storage, Nexus and external HTTP boundaries use
fixtures. No identity secret or public-network access is needed after dependencies
are installed. The fixture venue test requires a positive match. Samples with `group_with` must end as
one resource or in one group: an ISBN-10 tag and OSM spellings are aliases of the ISBN-13 and
the canonical OSM sample, and publisher pages group with the DOI and ISBN resources whose
registries name them as landing pages and which they cite. Samples with `maybe_same_with` are
one event on two pages: each card must name the other in `maybe_same` (possibly the same
event), and they must not group. Injected transports
disable direct relay sockets; unknown fixture DNS names resolve to TEST-NET.

Both `--mock` and `--isolated-live` create a unique `pinakes_e2e_owned_*` database
through `DATABASE_URL` (default `postgresql:///postgres?host=/var/run/postgresql`).
The role needs `CREATEDB`; the schema needs the extension privileges used by the test
suite. Only that newly created database is dropped on exit. They never reset the
development database. A force-killed process may leave its database for inspection.

## Live identity and dev stack

```sh
export PINAKES_E2E_SECRET_FILE=/absolute/path/to/32-byte-secret.key
export PINAKES_E2E_HOMESERVER=homeserver-public-key
# Only for a new identity: export PINAKES_E2E_SIGNUP_TOKEN=invite-token
export PINAKES_E2E_NEXUS=http://127.0.0.1:8080
export PINAKES_E2E_REPORT=.runtime/e2e-live.json
CARGO_PROFILE_DEV_DEBUG=0 scripts/e2e --isolated-live
```

`--isolated-live` uses the real SDK, homeserver, Nexus and public upstreams. Its private Pinakes
driver reads the Nexus feed but resolves only the samples it is given (the live samples below by
default, at most 128, one at a time), through the production queue path (batched completion,
leases and resolver policy), and runs the also-on sources for them. Music samples must be typed and
named by what they are (Spotify, Apple Music, TIDAL, Bandcamp, SoundCloud; `name` checks a card's
name, Daft Punk's TIDAL page); the Spotify track sample must show Deezer as a derived link, found
through MusicBrainz, the Wikidata item for Paris its French and English Wikipedia editions, and the
Berlin node (tagged `wikidata=Q64`, which names it back) its Wikidata page and English Wikipedia
article, and Discogs' OK Computer master its Apple Music album, through the Wikidata item
MusicBrainz relates its release group to, and Discogs' Radiohead its Deezer page. A tagged BTC Map
merchant page joins the OSM way it names (LAC Lugano). The music cases join (`group_with`): TIDAL's
`/u` share link of Bohemian Rhapsody with the seed's recording through its own ISRC, Cut to the
Feeling's Spotify and Deezer albums through their MusicBrainz release, Dracula's TIDAL, Apple and
Deezer albums through their one release group, MusicBrainz's recording and ISRC pages with TIDAL's
share link and its artist page with TIDAL's artist, a TIDAL album with Apple Music's through their
barcode, and Discogs' OK Computer master and Radiohead artist with their Spotify pages; a TIDAL
track whose ISRC nobody else knows keeps it in its `facts`. A fetch can show a sample to be another
link's spelling (a Bluesky post's handle URL, under its DID); the driver then resolves that link in
its place. Canaries and unrelated backlog fetching are off.

The driver reads its settings from the environment as `pinakesd` does (`PINAKES__…` and the named
secrets). A sample marked `"needs": "tidal"` (the barcode and own-ISRC cases, and the MusicBrainz
samples whose group joins TIDAL's) needs the TIDAL source, so set `PINAKES_TIDAL_CLIENT_ID` and
`PINAKES_TIDAL_CLIENT_SECRET`; without them the driver refuses to start and names those samples.
A build without the source at all leaves them out, in both modes, and the run names each one
(`left out: …`, and the report's `skipped`). The walled article (`feed`) names its site's RSS feed
(`latest_in`), and the harness tags that feed's newest item: the card comes from the feed, which
lists recent items only. A sample marked `needs_page` (the Podcast Index page) is a card read from
its page: when the site answers this host 401 or 403, the harness leaves it out of the run and
reports why (`skipped` in the report); every other sample stays strict. A full run of the default
samples takes three to eight minutes; each wait for Pinakes has its own deadline of ten minutes
(`PINAKES_E2E_TIMEOUT_MS`). Use this mode for a scoped live smoke test. Run `scripts/e2e` without a
mode to use an already configured dev stack at `PINAKES_E2E_PINAKES` (default
`http://127.0.0.1:8090`). The checks of a page before the run (`skipped`) send the User-Agent
Pinakes will send: an isolated stack's is this checkout's version with the operator's contact
(`PINAKES__HTTP__CONTACT`), else the run's; a running instance's is the one its `/v1/info`
reports (`user_agent`), whatever its file sets, so it must be a build that reports it.

The harness creates a minimal `/pub/pubky.app/profile.json` only when missing,
including on an existing identity; an existing profile is never overwritten. The live samples
also tag a Pubky object of another app, `pubky://{author}/pub/pinakes.e2e/objects/sample`
(`{author}` is the test identity's key), created the same way when missing and kept, which
Pinakes fetches from the homeserver's HTTPS domain through its egress policy. It has no file
extension, so the homeserver serves its JSON as `application/octet-stream`, as it does Mapky's. A new
identity needs the signup token. Signup/sign-in failures are redacted. The secret is read only from the named file, never logged.
Do not commit secrets, session state or invite tokens. SDK sessions are signed out
at the end.

Tags use **`/pub/pinakes.e2e/tags/<SDK-id>`**, because Nexus's universal-tag handler
accepts external URIs in non-`pubky.app` namespaces. The label is unique to the run.
The harness refuses existing paths and journals each attempted write before PUT,
so cleanup includes writes whose response was lost. Cleanup runs after failures
and SIGINT/SIGTERM as well as success, retries deletion, and records each result.
If the process is killed or cleanup fails, use the printed journal with the same
identity to delete only its listed paths; do not clear the whole namespace.

The live samples are `samples.live.json` and every `samples/<name>.json` (each source's own,
removed with the source). `PINAKES_E2E_SAMPLES` may select one JSON file of samples instead;
update stale public samples explicitly. The ICS default is
Google’s published Swiss holiday calendar, public by design with no capability
token. `PINAKES_E2E_ICS_URL` optionally selects another public feed; never commit
Outlook/iCloud/Nextcloud/Proton share URLs or capability tokens.
`PINAKES_E2E_TIMEOUT_MS`
sets the per-stage polling deadline (default 120,000 ms; 600,000 ms live, where the music samples
wait for MusicBrainz at one request a second). Terminal missing/error/
unreachable samples fail immediately; live mode never substitutes fixture data. A live event
with an accepted venue must produce a positive `venue=` match; an unresolved venue
may produce zero matches. Facet counts, each bbox/window row, fixture recurrence
count and created-change classes are checked. Tagged search waits for reference
ingestion even when a sample document is already cached; collapse waits for reconciliation. Reports contain public IDs/URIs and cleanup
paths, never the secret. Journals stay under ignored `.runtime/` with mode 0600.
