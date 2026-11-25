# GitLab

Pinakes reads projects, branches, tags, releases and files on gitlab.com and any other GitLab
instance from the instance's REST API (`/api/v4/projects/…`). No key is needed. Pinakes reads
project pages on a list of known instances, and links with GitLab's `/-/` shapes on any host,
whose answer then tells whether it runs GitLab.

| | |
| --- | --- |
| Source | `gitlab`, official source |
| Links | `https://{host}/{namespace}/{project}`, and `/-/tree/…`, `/-/blob/…`, `/-/tags/{tag}`, `/-/releases/{tag}` below it |
| Fetches | `GET https://{host}/api/v4/projects/{namespace%2Fproject}` and the `…/repository/branches`, `…/repository/tags`, `…/repository/commits` and `…/releases` records below it |
| Credentials | None |
| Rate | gitlab.com's 500 unauthenticated API requests a minute, declared as 2 a second on `gitlab.com`: above the shared default (`limits.host_requests_per_second`), which therefore applies. Other instances: the shared default |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareSourceCode` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.gitlab] enabled`, on by default |

## Links

Only `https` links without a port are read. These shapes are read:

| Object | Link |
| --- | --- |
| Project | `https://{host}/{namespace}/{project}`, also with `.git` |
| Branch, tag or commit (its tree) | `…/{namespace}/{project}/-/tree/{ref}` |
| File or directory at a ref | `…/-/tree/{ref}/{path}`, `…/-/blob/{ref}/{path}` |
| Tag | `…/-/tags/{tag}` |
| Release | `…/-/releases/{tag}` |

Groups nest, so a namespace can have several segments: a project path has 2 to 8 segments in all.
Each segment is letters, digits, `-`, `_` and `.`, not starting with `.` or `-`. A first segment
that is one of the instance's own pages (`explore`, `groups`, `users`, `api`, `help`, `-` and
others) is never a namespace.

Not read here: issues, merge requests and every other `/-/` page. They get web cards.

### Roots and aliases

The address the API gives a project (`web_url`) is its root. Every other spelling becomes an
[alias](../api.md#aliases) of it after its first fetch. On the known instances (below), the
`.git` form is an alias from the link alone.

The root of a branch or tag is its tree, `…/-/tree/{ref}`; of a tag read by its tag page,
`…/-/tags/{tag}`; of a file, `…/-/blob/{ref}/{path}` or `…/-/tree/{ref}/{path}`, as linked; of a
release, `…/-/releases/{tag}`.

### Which hosts are read

Project pages are read on the known instances: gitlab.com, framagit.org, gitlab.freedesktop.org,
gitlab.gnome.org, invent.kde.org and salsa.debian.org. They are also read on any host that
answered as GitLab within the last day.

The `/-/` shapes are read on any host. A host answers as GitLab when it gives the object asked for
(a project with its path, a branch or tag with its commit, a release with its tag, a list of
commits), or GitLab's own error (`{"message": "404 …"}`). A host that answers otherwise, fails or
does not answer is remembered for a day, in memory, and its links get ordinary web cards. A known
instance is never marked as not GitLab.

A page shaped like a project that the API does not know (a group, a user, an old page address)
gets an ordinary web card. So does one on a host learned this way whose answer is not a project of
that host.

### Refs with slashes

A ref can contain `/`, so the API decides where the ref ends and the path begins. Pinakes tries the
first one, two or three segments after `tree` or `blob` as a branch, then as a tag, shortest first.
A `blob` link keeps at least one segment for the path. A 40-character commit ID is the ref by
itself. When none of the tries is a ref and the link has more segments, the ref may be a longer
name: the link gets an ordinary web card.

## What it fetches

All requests go to the instance's API, `https://{host}/api/v4/projects/{namespace%2Fproject}`
(`{api}` below):

| Object | Requests |
| --- | --- |
| Project | `GET {api}` |
| Branch or tag tree | `GET {api}/repository/branches/{ref}`, then `GET {api}/repository/tags/{ref}`, for each try |
| File or directory | The ref as above, then its latest commit: `GET {api}/repository/commits?ref_name={ref}&path={path}&per_page=1` |
| Tag | `GET {api}/repository/tags/{tag}` |
| Release | `GET {api}/releases/{tag}` |

A link whose ref is a commit ID skips the branch and tag requests and asks only for the latest
commit. The file itself is never fetched.

The answers mean:

- A tag, release, ref or path the project does not have (404, or no commit that touches the path):
  `missing`.
- A project the API answers 404 for: an ordinary web card.
- A 408, 429 or 5xx: read again after at least a minute.
- Any other status, a release answer for another tag, or an answer without the fields the card
  needs: an error, and the read is retried.

## Card

The card is a `SoftwareSourceCode`. `site` is the instance's host. No commit author's name or
address is published, and an e-mail address in a description is removed. A description taken from
a commit or tag message is its first line, at most 200 characters.

| Object | Name | Description |
| --- | --- | --- |
| Project | Its path, such as `inkscape/inkscape` | The project's description; its avatar is the image |
| Branch or tag tree | `{project} · {ref}` | The latest commit's title; for a tag, its message first |
| Tag page | `{project} · {tag}` | The tag's message, else its commit's title |
| Commit tree | `{project} · {first 10 characters of the commit ID}` | The commit's title |
| File or directory | `{path} · {project}` | The title of the latest commit that changed it |
| Release | `{project} {release name}` (the tag without a name) | The start of its notes, at most 200 characters |

| Fact | Meaning |
| --- | --- |
| `stars` | A project's number of stars |
| `forks` | A project's number of forks |
| `topics` | A project's topics |
| `default_branch` | A project's default branch |
| `updated` | A project's last activity |
| `archived` | `true` for an archived project |
| `fork_of` | For a fork, its parent's path |
| `repository` | For a ref, file, tag or release, the project's page |
| `branch`, `tag` | The branch or tag the link names |
| `commit` | The ref's latest commit (for a file, the latest that changed it) |
| `committed` | When that commit was made |
| `path` | For a file or directory, its path |
| `published` | For a release, its date |
| `upcoming` | For a release, `true` while it is still upcoming |
| `page_url` | The object's page on the instance |

## Keys and groups

GitLab cards have no keys and join no [groups](../api.md#groups). `fork_of` and `repository` are
facts only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.gitlab]
enabled = false
```

GitLab links then get ordinary web cards.

## Examples

- `https://gitlab.com/inkscape/inkscape`
- `https://gitlab.com/inkscape/inkscape/-/blob/master/README.md`
- `https://gitlab.com/inkscape/inkscape/-/releases/INKSCAPE_1_4`
