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

Group origin checks use the compiled `psl = 2.1.241` public suffix data pinned in
Cargo.lock, including private suffixes. The dependency’s Cargo manifest declares MIT/Apache-2.0;
its source and license ship in the crate.
WordPress tenant hosts are treated separately even though that boundary is absent
from the list. No suffix data is fetched at runtime.
