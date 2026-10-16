# Pinned vocabulary

`schemaorg-30.1.jsonld` is the unmodified schema.org 30.1 current HTTPS vocabulary,
downloaded from <https://schema.org/version/30.1/schemaorg-current-https.jsonld>.
`schemaorg-types.json` derives every class and its direct parents from that file;
regenerate it with `scripts/schema-types`. The runtime computes the full ancestor
closure, preserving multiple inheritance. Unknown types are not invented.

Both vocabulary files are licensed under **Creative Commons Attribution-ShareAlike
3.0**, separately from Pinakes's MIT source code. Attribution: Schema.org and its
sponsors. See [schema.org terms](https://schema.org/docs/terms.html) and the
[license text](LICENSE.schemaorg.txt). The derived table removes properties and
comments and shortens `schema:` identifiers; it does not change the hierarchy.

`type-mappings.json` is Pinakes's MIT-licensed mapping of OpenGraph, oEmbed and
microformats names into the pinned vocabulary. Bumping the vocabulary is an
explicit repository change, followed by re-resolution to update stored paths.


## Calendar zones

`windows-zones.json` is derived from Unicode CLDR **47** `windowsZones.xml`, keeping
only territory `001` mappings (Windows name to the first IANA identifier). Source:
<https://raw.githubusercontent.com/unicode-org/cldr/release-47/common/supplemental/windowsZones.xml>.
Source SHA-256: `39714d7a5a276508aeca2289a490dddb8424dea0063f3f5fbc8bd8520dcbfcba`.
Derived SHA-256: `7ab89dbcedfcc61b06c03339bf86d32046cf9408c68a50d0a86e4ab02782a480`.
Unicode/CLDR attribution and terms are preserved in [LICENSE.cldr.txt](LICENSE.cldr.txt).
Regenerate with `scripts/windows-zones /path/to/windowsZones.xml` after downloading
that exact release. It validates the source digest and performs no network request.

IANA rules are pinned through `chrono-tz = 0.10.4`. Coordinate lookup uses
`tzf-rs = 1.1.4` with `tzf-rel = 0.0.2025-c` in Cargo.lock (Timezone Boundary Builder
2025c with oceans). The lookup data is bundled by that dependency, never downloaded
at runtime. It is unmodified ODbL data, separately licensed from this repository's
source: [LICENSE.tzf-rel.md](LICENSE.tzf-rel.md). Attribution: Timezone Boundary
Builder and OpenStreetMap contributors. Source and regeneration instructions:
<https://github.com/ringsaturn/tzf-rel/tree/2025c> and
<https://github.com/evansiroky/timezone-boundary-builder/releases/tag/2025c>.
Coordinate-derived `tz` values carry `timezone_attribution`; setting
`event.coordinate_timezone=false` disables this inference. Never treat an offset
alone as an IANA zone. Source upgrades are explicit lockfile/data changes.

`rails-zones.json` derives ActiveSupport TimeZone::MAPPING from Rails **8.1.0**,
for RingCentral/Hopin names. Source: <https://raw.githubusercontent.com/rails/rails/v8.1.0/activesupport/lib/active_support/values/time_zone.rb>.
Source SHA-256: `c20a9fb54a413919aa8944649d7424e6dbc1e4b5c95cd02a1b169682bfbb0285`.
The table preserves all 152 mappings; IANA rules still come from chrono-tz.
Copyright Rails contributors, MIT; see [LICENSE.rails.txt](LICENSE.rails.txt).
Regenerate with `scripts/rails-zones /path/to/time_zone.rb`.

## oEmbed providers

`oembed-providers.json` pins the 390-provider registry downloaded from
<https://oembed.com/providers.json> on 2026-10-16. SHA-256:
`274d3cadad4ebb51fec4303b422e84128bcac988d4275289448297c205dab19d`.
The registry project is <https://github.com/iamcal/oembed>; its MIT license is
preserved as [oembed-LICENSE](oembed-LICENSE). This is a separate upstream data
license. Updates require an explicit replacement/digest review and resolver tests.

The media resolver matches registry URL schemes locally, uses JSON-capable provider
endpoints, and applies normal DNS/redirect/size/host budgets to every request. It
retains typed metadata and origin image URLs only; it never serves embed HTML or
rehosts media. Provider membership is not a promise that its public endpoint is
available or permits a particular use.
