# Forgejo and Gitea

Repositories, branches, tags, releases and files on Forgejo and Gitea instances: Codeberg,
gitea.com and any other. Everything is read from the instance's API (`/api/v1/repos/…`), never
from its pages: Codeberg answers deep pages with a bot challenge, which is never solved. No key;
Codeberg allows 2,000 requests per 10 minutes (one per second here).

## Links

- a **repository**: `https://{host}/{owner}/{repo}` (the root) and `/{repo}.git`;
- a **branch, tag or commit**: `/{owner}/{repo}/src/branch/{branch}`, `/src/tag/{tag}`,
  `/src/commit/{sha}`;
- a **file or directory** at one of them: `/src/branch/{branch}/{path}`, and the same under
  `tag` and `commit`;
- a **release**: `/{owner}/{repo}/releases/tag/{tag}`.

A branch name can contain `/` (`v11.0/forgejo`), so the split between branch and path is asked
of the API: the first one, two or three segments are tried as the branch (or tag), shortest
first. When none of them is a branch (or tag) and the link has more segments, the name may be a
longer one: the link gets an ordinary web card.

Repository pages and release links (`/releases/tag/…`, a shape GitHub's release links have too)
are read on codeberg.org and gitea.com, and on any host that answered as an instance within the
last day. The `/src/…` shapes (`/src/branch/…`, `/src/tag/…`, `/src/commit/…`) are read on
any host: an unknown host is first asked for `/api/v1/version`, which must answer exactly as
Forgejo and Gitea do (`{"version": "…"}`, the value a version such as `1.22.0` or
`11.0.1+gitea-1.22.0`). The answer is remembered for a day, in memory: a host that answers
otherwise, fails or does not answer gets ordinary web cards. On a host learned this way, a
two-segment page the API does not know as a repository gets an ordinary web card too. Pages
under `/explore`, `/user`, `/api` and the instance's other own paths are not read, nor issues
and pull requests.

The address the API gives a repository (`html_url`) is its root; every other spelling (another
letter case, `.git`) becomes an alias of it after its first fetch. On codeberg.org and gitea.com
`.git` is an alias from the link alone.

## What it reads

- A repository: `GET /api/v1/repos/{owner}/{repo}` (about 3 KB).
- A branch or tag: `GET …/branches/{name}` or `…/tags/{name}` (about 1 KB each try).
- A file, a directory or the tree at a commit: the ref as above, then its latest commit,
  `GET …/commits?sha={ref}&path={path}&limit=1&stat=false&files=false&verification=false`
  (about 3 KB). The file itself is never fetched.
- A release: `GET …/releases/tags/{tag}`.

A ref, path or release the instance does not have is `missing`, and so is a repository on
codeberg.org or gitea.com.

## Card

`SoftwareSourceCode`. A repository is named `{owner}/{repo}` and described by its description,
with its avatar as the image; a branch or tag `{owner}/{repo} · {name}`, described by its
commit's first line; a file `{path} · {owner}/{repo}`, described by the first line of the latest
commit that changed it; a release `{owner}/{repo} {release name}`, described by the start of its
notes. No commit author's name or address is published, and an e-mail address in a description
is left out.

| Fact | Meaning |
| --- | --- |
| `stars`, `forks`, `language`, `topics` | For a repository |
| `default_branch`, `updated` | Its default branch; when it last changed |
| `archived` | `true` for an archived repository |
| `mirror_of` | For a mirror: the repository it mirrors |
| `fork_of` | For a fork: its parent, `{owner}/{repo}` |
| `website` | The repository's website |
| `repository` | For a branch, tag, file or release: the repository's page |
| `branch`, `tag` | The ref named in the link |
| `commit`, `committed` | The ref's latest commit (for a file, the latest that changed it) and when it was made |
| `path` | For a file or directory |
| `published`, `prerelease` | For a release |
| `page_url` | Its page on the instance |

## Examples

- `https://codeberg.org/forgejo/forgejo`
- `https://codeberg.org/forgejo/forgejo/src/branch/forgejo/README.md`
- `https://codeberg.org/forgejo/forgejo/releases/tag/v11.0.1`
