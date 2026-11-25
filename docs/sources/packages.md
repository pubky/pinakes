# `packages`: a package's registry pages, provenance and archive

This lookup family of [also on](../api.md#also-on) runs on packages on crates.io, npm and PyPI, and
on GitHub and GitLab repositories. It adds a package's other pages and the repository that signed
build provenance names, and a repository's archive page.

| | |
| --- | --- |
| Source | `packages`, lookup family with sources `crates`, `npm`, `pypi`, `software_heritage` |
| Runs on | `crates`, `npm`, `pypi`: a package's own page (below). `software_heritage`: a GitHub or GitLab repository's own page |
| Asks | `https://crates.io/api/v1/crates/{name}`; `https://registry.npmjs.org/{name}/latest` and its attestations; `https://pypi.org/pypi/{name}/json` and its file provenance; `https://api.deps.dev/v3/…`; `https://archive.softwareheritage.org/api/1/origin/{url}/get/` |
| Credentials | None |
| Rate | At most 1 request per second on `crates.io` and 120 per hour on `archive.softwareheritage.org`. The other hosts: shared per-host default, 1 request per second (`limits.host_requests_per_second`) |
| Rows live | 30 days; rows expire after twice that |
| Keys | `crate:{name}`, `npm:{name}`, `pypi:{name}`, `repo:{github.com\|gitlab.com}/{path}` |
| Links | The package's pages on its registry and documentation sites, its source repository (`kind` `source`), and a repository's Software Heritage page. All are exact, and none carries a key |
| Facts | None |
| Switch | `[also_on.sources.<id>] enabled` for each of the four sources, on by default |

## What it does

### Pages it reads

| Registry | Page |
| --- | --- |
| crates.io | `crates.io/crates/{name}[/{version}]` |
| | `docs.rs/{name}`, `docs.rs/{name}/{version}`, `docs.rs/{name}/{version}/{name}` (the crate's index), `docs.rs/crate/{name}[/{version}]` |
| | `lib.rs/crates/{name}` |
| npm | `npmjs.com/package/{name}[/v/{version}]` |
| | `jsdelivr.com/package/npm/{name}` |
| PyPI | `pypi.org/project/{name}[/{version}]` |

A version starts with a digit. `latest` also counts on crates.io, docs.rs and PyPI. npm scopes
(`@scope/name`, the `@` encoded or not) are read. A page under a package names another object and is
not read: a function's documentation, a source file, a dependency list.

A repository is `https://github.com/{owner}/{name}` or
`https://gitlab.com/{group}/…/{name}`, with or without `.git`. Its page stays the repository's with a
fragment (`#readme`) or a query that only picks a tab or marks where the link was shared from:
`tab`, `ref`, `ref_src`, `ref_type`, `source`, `fbclid`, `gclid`, `utm_*`. Any other query is another
page. The forge's own pages (`github.com/topics/rust`, `gitlab.com/explore`, `gitlab.com/groups/…`, a
search) and a repository's issues, branches and files are no repository.

### `crates`, `npm` and `pypi`

1. The source asks the registry for the package. It must answer for the same name. A package the
   registry does not know gives nothing.
2. The card gets the package's key and its pages as links.
3. Its source repository becomes a link when signed build provenance names exactly one:

| Source | Provenance |
| --- | --- |
| `crates` | The trusted-publishing record crates.io keeps for the crate's default version |
| `npm` | The SLSA provenance of the latest release. Its subject must be this package and version, and it must name one repository. Attestations are read from `registry.npmjs.org` only |
| `pypi` | The trusted publisher of up to two files of the latest release, wheels first. A file whose attestations disagree names none, and neither do files that name different repositories |

4. When that names no repository, the source asks deps.dev for the release. The one repository that
   deps.dev links through an `SLSA_ATTESTATION` as `SOURCE_REPO` is the repository.

The repository must be on GitHub or GitLab. A package's own `repository` field is the publisher's
claim and is never shown.

### `software_heritage`

1. A GitHub or GitLab repository gets the key `repo:{host}/{path}`.
2. The card gets its Software Heritage archive page as a link when Software Heritage answers for that
   repository.

## Keys, links and facts

| Key | Source | Normalized as |
| --- | --- | --- |
| `crate:{name}` | `crates` | Lower case, `_` as `-` |
| `npm:{name}` | `npm` | As written |
| `pypi:{name}` | `pypi` | Lower case, runs of `-`, `_` and `.` as one `-` (PEP 503) |
| `repo:{host}/{path}` | `software_heritage` | Lower case |

Each key names one object, so a [group](../api.md#groups) holds at most one value of each, and
[`key=`](../api.md#search) finds the links that carry it. The value is normalized in the same way:
`key=crate:Serde_JSON` finds `crate:serde-json`.

| Source | Links |
| --- | --- |
| `crates` | crates.io, Docs.rs, lib.rs, and the repository |
| `npm` | npm, jsDelivr, unpkg, and the repository |
| `pypi` | PyPI, and the repository |
| `software_heritage` | The archive page (`via` `softwareheritage`) |

The repository is never a key. One repository often publishes several packages (a workspace, a
monorepo), so it joins none of them.

A card on these hosts is named after its service (`site`) only on an object's own page: crates.io,
Docs.rs, lib.rs, npm, jsDelivr, unpkg, PyPI, GitHub, GitLab and Software Heritage. unpkg's page for a
package (`unpkg.com/{name}/`) is named, but no source here runs on it.

## Cost

| Source | Requests per card |
| --- | --- |
| `crates` | One to crates.io, and one to deps.dev when crates.io names no repository |
| `npm` | One to the registry, one more for attestations when it has them, and one to deps.dev when they name no repository |
| `pypi` | One to PyPI, up to two more for file provenance, and one to deps.dev when they name no repository |
| `software_heritage` | One |

Answers are cached for `limits.discovery_cache_secs` (an hour by default).

When the provenance, deps.dev or Software Heritage fails, the card keeps its key and what else was
found, its stored rows stay, and the run comes back after a failed run's backoff
([operations](../operations.md#runs)). A failure of the registry itself fails the source's run, which
is retried.

## Turn it off

Switch off each source on its own:

```toml
[also_on.sources.crates]
enabled = false

[also_on.sources.npm]
enabled = false

[also_on.sources.pypi]
enabled = false

[also_on.sources.software_heritage]
enabled = false
```

After the restart, a source's stored keys and links stop showing, and groups leave its keys out.
They are kept until they expire and show again when the source is on. A repository's own
`repo:` key is recorded by `software_heritage`, so it goes with that source.
