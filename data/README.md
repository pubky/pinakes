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
