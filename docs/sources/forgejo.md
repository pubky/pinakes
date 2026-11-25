# Forgejo and Gitea

Pinakes reads repositories, branches, tags, releases and files on Forgejo and Gitea instances
(Codeberg, gitea.com and any other) from the instance's REST API (`/api/v1/repos/…`), never from
its pages: Codeberg answers deep pages with a bot challenge, which Pinakes never solves. No key is
needed.

| | |
| --- | --- |
| Source | `forgejo`, official source |
| Links | `https://{host}/{owner}/{repo}`, `…/src/branch/…`, `…/src/tag/…`, `…/src/commit/…`, `…/releases/tag/{tag}` |
| Fetches | `GET https://{host}/api/v1/repos/{owner}/{repo}` and the `…/branches`, `…/tags`, `…/commits` and `…/releases/tags` records below it; `GET https://{host}/api/v1/version` for an unknown host |
| Credentials | None |
| Rate | Codeberg's 2,000 requests per 10 minutes, declared as 2 a second on `codeberg.org`: above the shared default (`limits.host_requests_per_second`), which therefore applies. Other instances: the shared default |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareSourceCode` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.forgejo] enabled`, on by default |

## Links

Only `https` links without a port are read. These shapes are read:

| Object | Link |
| --- | --- |
| Repository | `https://{host}/{owner}/{repo}`, also with `.git` |
| Branch | `…/{owner}/{repo}/src/branch/{branch}` |
| Tag | `…/src/tag/{tag}` |
| Commit (its tree) | `…/src/commit/{sha}` |
| File or directory | `…/src/branch/{branch}/{path}`, and the same under `tag` and `commit` |
| Release | `…/{owner}/{repo}/releases/tag/{tag}` |

An owner or repository name is 1 to 100 letters, digits, `-`, `_` and `.`, not starting with `.`.
A commit ID is 7 to 64 hexadecimal characters. A first segment that is one of the instance's own
pages (`explore`, `user`, `api`, `org`, `repo`, `issues`, `pulls`, `admin` and others) is never an
owner.

Not read here: issues, pull requests and every other repository page. They get web cards.

### Roots and aliases

The address the API gives a repository (`html_url`) is its root. Every other spelling, such as
another letter case or `.git`, becomes an [alias](../api.md#aliases) of it after its first fetch.
On codeberg.org and gitea.com, the `.git` form is an alias from the link alone.

The root of a branch, tag or commit is `…/src/{branch|tag|commit}/{ref}`, with the path after it
for a file or directory; of a release, `…/releases/tag/{tag}`.

### Which hosts are read

Repository pages and release links are read on codeberg.org and gitea.com, and on any host that
answered as an instance within the last day. Release links have the same shape as GitHub's, so
they are not read on unknown hosts.

The `/src/…` shapes are read on any host. An unknown host is first asked for `/api/v1/version`,
which must answer exactly as Forgejo and Gitea do: `{"version": "…"}` and nothing else, the value
a version such as `1.22.0` or `11.0.1+gitea-1.22.0`. The answer is remembered for a day, in
memory. A host that answers otherwise, fails or does not answer gets ordinary web cards. On a host
learned this way, a two-segment page the API does not know as a repository gets an ordinary web
card too.

### Branch names with slashes

A branch or tag name can contain `/`, as in `v11.0/forgejo`, so the API decides where the name
ends and the path begins. Pinakes tries the first one, two or three segments after `branch` or
`tag` as that kind of ref, shortest first. A commit's ref is always its first segment. When none of
the tries is a ref and the link has more segments, the name may be a longer one: the link gets an
ordinary web card.

## What it fetches

All requests go to the instance's API, `https://{host}/api/v1/repos/{owner}/{repo}` (`{api}`
below):

| Object | Requests |
| --- | --- |
| Repository | `GET {api}` |
| Branch | `GET {api}/branches/{name}`, for each try |
| Tag | `GET {api}/tags/{name}`, for each try |
| File, directory, or the tree at a commit | The ref as above (not for a commit), then its latest commit: `GET {api}/commits?sha={ref}&path={path}&limit=1&stat=false&files=false&verification=false` |
| Release | `GET {api}/releases/tags/{tag}` |

For an unknown host, the first read also costs `GET https://{host}/api/v1/version`. The file
itself is never fetched.

The answers mean:

- A ref, path or release the instance does not have (404, or no commit that touches the path):
  `missing`.
- A repository the API answers 404 for: `missing` on codeberg.org and gitea.com, an ordinary web
  card on other hosts.
- A 408, 429 or 5xx: read again after at least a minute.
- Any other status, a release answer for another tag, or an answer without the fields the card
  needs: an error, and the read is retried.

## Card

The card is a `SoftwareSourceCode`. `site` is the instance's host. No commit author's name or
address is published, and an e-mail address in a description is removed. A description taken from
a commit or tag message is its first line, at most 200 characters.

| Object | Name | Description |
| --- | --- | --- |
| Repository | `{owner}/{repo}` | The repository's description; its avatar is the image |
| Branch | `{owner}/{repo} · {branch}` | The commit's message |
| Tag | `{owner}/{repo} · {tag}` | The tag's message |
| Commit tree | `{owner}/{repo} · {first 10 characters of the commit ID}` | The commit's message |
| File or directory | `{path} · {owner}/{repo}` | The message of the latest commit that changed it |
| Release | `{owner}/{repo} {release name}` (the tag without a name) | The start of its notes, at most 200 characters |

| Fact | Meaning |
| --- | --- |
| `stars` | A repository's number of stars |
| `forks` | A repository's number of forks |
| `language` | A repository's main language |
| `topics` | A repository's topics |
| `default_branch` | A repository's default branch |
| `updated` | When the repository last changed |
| `archived` | `true` for an archived repository |
| `mirror_of` | For a mirror, the address of the repository it mirrors |
| `fork_of` | For a fork, its parent, `{owner}/{repo}` |
| `website` | The repository's website |
| `repository` | For a branch, tag, commit, file or release, the repository's page |
| `branch`, `tag` | The branch or tag the link names |
| `commit` | The ref's commit (for a file, the latest that changed it) |
| `committed` | When that commit was made |
| `path` | For a file or directory, its path |
| `published` | For a release, its date |
| `prerelease` | `true` for a pre-release |
| `page_url` | The object's page on the instance |

## Keys and groups

Forgejo and Gitea cards have no keys and join no [groups](../api.md#groups). `mirror_of`,
`fork_of`, `website` and `repository` are facts only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.forgejo]
enabled = false
```

Forgejo and Gitea links then get ordinary web cards.

## Examples

- `https://codeberg.org/forgejo/forgejo`
- `https://codeberg.org/forgejo/forgejo/src/branch/forgejo/README.md`
- `https://codeberg.org/forgejo/forgejo/releases/tag/v11.0.1`
