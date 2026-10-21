# `packages`: a package's registry pages, provenance and archive

A lookup family of [also on](../api.md#also-on) (`lookups/packages.rs`).

A package's own page on crates.io (and docs.rs, lib.rs), npm (and jsDelivr) or
PyPI, or its page at one version; a page under it (a function's documentation, a source file, a
dependency list) is another object:
- `crates`, `npm`, `pypi`: the package's pages on its registry and documentation sites, its name
  as key (`crate:`, `npm:`, `pypi:`, normalized as each registry does), and its source repository (`kind: "source"`)
  only when signed build provenance names it (crates.io trusted publishing, npm SLSA provenance,
  PyPI trusted publishers, else deps.dev's `SLSA_ATTESTATION`), as a link only: one repository
  often publishes several packages (a workspace, a monorepo), so it joins none of them. A
  package's own `repository` field is the publisher's claim and is never shown.
- `software_heritage`: a GitHub or GitLab repository's own `repo:` key
  (`key=repo:github.com/{owner}/{name}`), and its Software Heritage archive page when Software
  Heritage archives it.


A card on these hosts is named after its service (`site`: crates.io, Docs.rs, lib.rs, npm,
jsDelivr, unpkg, PyPI, GitHub, GitLab, Software Heritage) only when it is such an object's own
page. A repository's page keeps its name with a fragment (`#readme`) or a query that only picks a
tab or marks where the link was shared from (`?tab=readme-ov-file`, the link GitHub's share
button gives; `ref`, `ref_src`, `ref_type`, `source`, `fbclid`, `gclid`, `utm_*`); any other
query is another page. The forges' own pages (`github.com/topics/rust`, `gitlab.com/explore`,
`gitlab.com/groups/…`, a search) are no repository.

## Keys

`crate:{name}`, `npm:{name}`, `pypi:{name}` (each normalized as its registry does) and `repo:{github.com|gitlab.com}/{path}` (`software_heritage`): each names one object, so a [group](../api.md#groups) holds at most one value of each, and [`key=`](../api.md#search) finds the links that carry it.

## Operations

Each package costs one registry request, a PyPI package up to two more for its files'
provenance, an npm package one more for its attestations, and one to deps.dev when the registry
names no provenance. crates.io is paced at 1 request/s, Software Heritage at 120 requests/hour
(anonymous).
