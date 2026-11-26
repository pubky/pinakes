# Pinned vocabulary

This directory holds the data sets Pinakes compiles into its binary: the schema.org type table,
time zone tables, the oEmbed provider registry and Pinakes's own tables. The schema.org release the
type table is derived from is kept here too, but is not compiled in. Nothing here is fetched
at runtime. An upstream file is pinned by version and SHA-256, and you update it with a change to
the repository.

| File | What it is | Read by | How to update it |
| --- | --- | --- | --- |
| `schemaorg-30.1.jsonld` | schema.org 30.1 vocabulary, unmodified | `scripts/schema-types` only | Replace it with a new release, then regenerate `schemaorg-types.json` |
| `schemaorg-types.json` | Every schema.org class and its direct parents | `model.rs` (`type_path`) | `scripts/schema-types` |
| `local-types.json` | Pinakes's own card types | `model.rs` (`type_path`) | By hand |
| `type-mappings.json` | OpenGraph, oEmbed, microformats and OSM names mapped to types | `resolver/web.rs`, `resolver/osm.rs` | By hand |
| `windows-zones.json` | Windows time zone names to IANA names (CLDR 47) | `event/time.rs` | `scripts/windows-zones FILE` |
| `rails-zones.json` | Rails time zone names to IANA names (Rails 8.1.0) | `event/time.rs` | `scripts/rails-zones FILE` |
| `oembed-providers.json` | The oEmbed provider registry | `resolver/cards.rs` | `scripts/oembed-providers [FILE]` |
| `multi-tenant-hosts.json` | Which part of a shared host is one publisher | `group_keys.rs` (`TENANT_RULES`) | By hand |
| `api-recipes.json` | The named API examples | `recipes.rs`, `tests/recipes.rs`, `scripts/e2e --live-recipes` | By hand |
| `event-adapters.json` | One entry per event platform | `event/adapters/mod.rs` (and a test in `config.rs`) | By hand |
| `interstitial-markers.json` | Markers of a bot wall or challenge page | `resolver/interstitial.rs` | By hand |
| `spellings.json` | The tracking parameters Pinakes drops, test vectors of the URI-only spelling rules and the identifier pages ([docs/api.md](../docs/api.md#spelling-vectors)); its hash is `/v1/info` `spelling_rules` | `alias.rs` (`spelling_rules`, `TRACKING`, `web_page`), `tests/spellings.rs` | Add a link by hand, then copy over it the file `cargo test -p pinakes-core --test spellings` writes with this build's answers |

The licence files next to them (`LICENSE.schemaorg.txt`, `LICENSE.cldr.txt`, `LICENSE.rails.txt`,
`LICENSE.tzf-rel.md`, `oembed-LICENSE`) hold the upstream licence texts. Each upstream data set
is licensed separately from Pinakes's MIT source code.

## Types

### schema.org

`schemaorg-30.1.jsonld` is the schema.org 30.1 current HTTPS vocabulary, unmodified, downloaded
from <https://schema.org/version/30.1/schemaorg-current-https.jsonld>. SHA-256:
`0a4b8c4c910fc831ec8695196510973390906a5ee34dc6b862dfec4d21a12f74`.

`schemaorg-types.json` lists every `schema:` class in that file with its direct parents.
`scripts/schema-types` derives it: the script drops properties and comments and shortens the
`schema:` identifiers, and it keeps the hierarchy as it is. At runtime `model::type_path`
computes each type's full ancestor path from this table, keeping multiple inheritance. A type
name that is in neither this table nor `local-types.json` gets no path: Pinakes does not invent
types.

Both files are licensed under Creative Commons Attribution-ShareAlike 3.0, with attribution to
Schema.org and its sponsors. The licence text is in [LICENSE.schemaorg.txt](LICENSE.schemaorg.txt).

To move to a new schema.org release, replace the `.jsonld` file, update the path in
`scripts/schema-types`, run the script, and update the version and digest here. Stored type paths
change only when their cards are resolved again.

### Pinakes's own types

`local-types.json` names Pinakes's own card types, for objects schema.org has no class for:
`BitcoinAddress`, `BitcoinBlock`, `BitcoinTransaction`, `LightningChannel` and `LightningNode`.
They are not schema.org types. Each has `Thing` as its parent and joins the same type paths. A
name that schema.org already uses makes `type_path` panic the first time it loads the tables
(`a local type shadows schema.org`), so every test that reads a type fails.
To add a type, add a line here and its entry in [docs/api.md](../docs/api.md).

### Type mappings

`type-mappings.json` maps names from other vocabularies to types:

- `opengraph`: OpenGraph `og:type` values, such as `video.movie` to `Movie`.
- `oembed`: oEmbed response types.
- `microformats`: `h-event`, `h-card` and `h-entry`.
- `osm`: OpenStreetMap `key/value` tags, such as `amenity/cafe` to `CafeOrCoffeeShop`. A
  `key/*` entry (`shop/*`) covers every value of that key without an entry of its own.

The web resolver reads the first three, the OpenStreetMap resolver reads `osm`.

## Time zones

A calendar or event platform names a zone in one of three ways. `event::time::zone` tries them in
this order:

1. An IANA name (`Europe/Zurich`), also inside a longer prefixed name such as Outlook's
   `/freeassociation.sourceforge.net/Tzfile/Europe/Zurich`. The IANA rules come from
   `chrono-tz = 0.10.4` in `Cargo.lock`.
2. A Windows name (`W. Europe Standard Time`), from `windows-zones.json`.
3. A Rails name (`Eastern Time (US & Canada)`), from `rails-zones.json`.

A UTC offset alone is never treated as an IANA zone.

### Windows names

`windows-zones.json` is derived from Unicode CLDR 47 `windowsZones.xml`. It keeps only the
territory `001` mappings, each Windows name with the first IANA name listed for it.

- Source: <https://raw.githubusercontent.com/unicode-org/cldr/release-47/common/supplemental/windowsZones.xml>
- Source SHA-256: `39714d7a5a276508aeca2289a490dddb8424dea0063f3f5fbc8bd8520dcbfcba`
- Derived file SHA-256: `7ab89dbcedfcc61b06c03339bf86d32046cf9408c68a50d0a86e4ab02782a480`
- Licence and attribution: Unicode CLDR, in [LICENSE.cldr.txt](LICENSE.cldr.txt)

To regenerate the file, download that exact release and run
`scripts/windows-zones /path/to/windowsZones.xml`. The script refuses a file with another digest
and makes no network request.

### Rails names

`rails-zones.json` holds all 152 entries of ActiveSupport `TimeZone::MAPPING` from Rails 8.1.0.
The IANA rules still come from `chrono-tz`.

- Source: <https://raw.githubusercontent.com/rails/rails/v8.1.0/activesupport/lib/active_support/values/time_zone.rb>
- Source SHA-256: `c20a9fb54a413919aa8944649d7424e6dbc1e4b5c95cd02a1b169682bfbb0285`
- Licence: MIT, copyright Rails contributors, in [LICENSE.rails.txt](LICENSE.rails.txt)

To regenerate the file, run `scripts/rails-zones /path/to/time_zone.rb`. The script refuses a
file with another digest.

### Zones from coordinates

When an event has coordinates but no zone, Pinakes looks the zone up from the coordinates with
`tzf-rs = 1.1.4` and its data crate `tzf-rel = 0.0.2025-c` (Timezone Boundary Builder 2025c, with
oceans), both pinned in `Cargo.lock`. The boundary data ships inside the crate and is never
downloaded. It is unmodified ODbL data, licensed separately: see
[LICENSE.tzf-rel.md](LICENSE.tzf-rel.md). Its sources are
<https://github.com/ringsaturn/tzf-rel/tree/2025c> and
<https://github.com/evansiroky/timezone-boundary-builder/releases/tag/2025c>.

A card whose zone came from coordinates credits `Timezone Boundary Builder / OpenStreetMap
contributors (ODbL-1.0), tzf-rel 2025c` in its `attribution` (`event::time::ATTRIBUTION`).

To move to newer boundary data, update the crates in `Cargo.lock` and this section.

## oEmbed providers

`oembed-providers.json` pins the 390-provider registry downloaded from
<https://oembed.com/providers.json> on 2026-10-16. SHA-256:
`274d3cadad4ebb51fec4303b422e84128bcac988d4275289448297c205dab19d`.
The registry project is <https://github.com/iamcal/oembed>. Its MIT licence is in
[oembed-LICENSE](oembed-LICENSE), separate from Pinakes's.

The media resolver (`resolver/cards.rs`) matches a link against the registry's URL schemes
locally and asks only the providers' JSON endpoints. Each request goes through the egress client
with its DNS, redirect, body size and per-host limits. Pinakes keeps the typed metadata and the
origin's image URLs. It never serves the embed HTML and never rehosts media.

Four providers are skipped in code (`resolver::cards::SKIPPED_PROVIDERS`), so a registry update
cannot bring them back:

| Provider | Why it is skipped |
| --- | --- |
| SoundCloud | It answers server addresses with a bot challenge. |
| TikTok | It answers without metadata. |
| Facebook, Instagram | They need an app token. |

To update the registry, run `scripts/oembed-providers [FILE]`. Without `FILE` it downloads
<https://oembed.com/providers.json>. The script checks the file's shape, lists the providers
added and removed, writes the file byte for byte, and rewrites the date, count and SHA-256 in the
paragraph above. Keep that paragraph's wording and line breaks: the script finds it by pattern
and stops when it cannot. Review the diff, then run `scripts/test`.

## Shared hosts

When Pinakes asks whether two links come from the same publisher (`group_keys::same_origin`),
it compares their registrable domains. The public suffix list for that is compiled into
`psl = 2.1.241` (pinned in `Cargo.lock`, private suffixes included). The crate declares
MIT/Apache-2.0 and ships its source and licence. No suffix data is fetched at runtime.

Before the suffix list applies, `multi-tenant-hosts.json` splits a shared host into its
publishers (`group_keys::origin`). Each host has one rule:

| Rule | Publisher | Example hosts |
| --- | --- | --- |
| `"subdomain"` | The full host name | `wordpress.com`, `blogspot.com`, `github.io` |
| `"first_path_segment"` | The host and the first path segment | `medium.com`, `linktr.ee`, `feeds.libsyn.com` |
| `{"path_segment": N}` | The host and the first N path segments | `sites.google.com`, `feeds.acast.com` |
| `{"path": {...}}` | The host and the first `segment` path segments, after removing the `skip` patterns; when the last of them is listed in `extend`, one more; a shorter path falls back to the first `query` parameter present | `anchor.fm`, `calendar.google.com` |

A link with too few path segments (and no such query parameter) is a publisher of its own. A subdomain of a listed host takes
the host's rule; the longest matching host wins.
[Tenant rules](../docs/operations.md#tenant-rules) in the operations guide say where they apply.

## Pinakes's own tables

These tables have no upstream file and are MIT-licensed like the source code.

- `api-recipes.json`: the named API examples (`recipes.rs`). They become the Swagger UI and
  OpenAPI examples, `tests/recipes.rs` runs them, and `scripts/e2e --live-recipes` resolves the
  public `/v1/resolve` ones live. Each one is listed in [docs/api.md](../docs/api.md) or on its
  source's page, and a test checks that.
- `event-adapters.json`: one entry per event platform, 58 in all. An entry names the platform's
  domains and paths, its parser, mode and cadence floor, its identity and cancellation signal,
  the fields a card is expected to have, its canary link, and whether it needs a key
  (`requires_key`) or is `enabled`. [docs/operations.md](../docs/operations.md) describes the
  platforms; [docs/api-keys.md](../docs/api-keys.md) lists the one that needs a key.
- `interstitial-markers.json`: what marks a bot wall or challenge page instead of content
  (`resolver/interstitial.rs`). It has lists of texts, challenge texts and signatures,
  placeholder titles, CAPTCHA selectors, wall host prefixes and wall paths.
