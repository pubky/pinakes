# GitLab

Projects, branches, tags, releases and files on gitlab.com and any other GitLab instance, read
from the instance's API (`/api/v4/projects/…`). No key; gitlab.com allows 500 unauthenticated API
requests per minute (one per second here).

## Links

- a **project**: `https://{host}/{namespace}/{project}` (the root; groups nest, so the namespace
  can have several segments) and `/{project}.git`;
- a **branch, tag or commit's tree**: `/{namespace}/{project}/-/tree/{ref}`;
- a **file or directory** at a ref: `/-/tree/{ref}/{path}` and `/-/blob/{ref}/{path}`;
- a **tag**: `/-/tags/{tag}`;
- a **release**: `/-/releases/{tag}`.

A ref can contain `/`, so the split between ref and path is asked of the API: the first one, two
or three segments are tried as a branch, then as a tag, shortest first. A 40-character commit id
is the ref by itself.

Project pages are read on gitlab.com, framagit.org, gitlab.freedesktop.org, gitlab.gnome.org,
invent.kde.org and salsa.debian.org, and on any host that answered as GitLab within the last
day. The `/-/` shapes are read on any host; one that answers otherwise, fails or does not answer
is remembered for a day, in memory, and gets ordinary web cards. A known instance is never
marked as not GitLab. A page shaped like a project that the API does
not know (a group, a user, an old page address) gets an ordinary web card too. Issues and merge
requests are not read.

The address the API gives a project (`web_url`) is its root; every other spelling becomes an
alias of it after its first fetch. On the known instances `.git` is an alias from the link alone.

## What it reads

- A project: `GET /api/v4/projects/{namespace%2Fproject}` (about 1 KB).
- A branch or tag: `GET …/repository/branches/{ref}`, then `…/repository/tags/{ref}` (about 1 KB
  each try).
- A file or directory: the ref as above, then its latest commit,
  `GET …/repository/commits?ref_name={ref}&path={path}&per_page=1`. The file itself is never
  fetched.
- A tag: `GET …/repository/tags/{tag}`; a release: `GET …/releases/{tag}`.

A ref, path, tag or release the project does not have is `missing`.

## Card

`SoftwareSourceCode`. A project is named by its path (`inkscape/inkscape`) and described by its
description, with its avatar as the image; a ref `{project} · {ref}`, described by its latest
commit's title; a file `{path} · {project}`, described by the title of the latest commit that
changed it; a release `{project} {release name}`, described by the start of its notes. No commit
author's name or address is published.

| Fact | Meaning |
| --- | --- |
| `stars`, `forks`, `topics` | For a project |
| `default_branch`, `updated` | Its default branch; its last activity |
| `archived` | `true` for an archived project |
| `fork_of` | For a fork: its parent's path |
| `repository` | For a ref, file, tag or release: the project's page |
| `branch`, `tag` | The ref named in the link |
| `commit`, `committed` | The ref's latest commit (for a file, the latest that changed it) and when it was made |
| `path` | For a file or directory |
| `published`, `upcoming` | For a release: its date, and `true` while it is still upcoming |
| `page_url` | Its page on the instance |

## Examples

- `https://gitlab.com/inkscape/inkscape`
- `https://gitlab.com/inkscape/inkscape/-/blob/master/README.md`
- `https://gitlab.com/inkscape/inkscape/-/releases/INKSCAPE_1_4`
