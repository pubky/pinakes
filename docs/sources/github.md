# GitHub

Repositories on github.com, read from GitHub's REST API with the operator's token. The source is
off without a token: GitHub links then get ordinary web cards.

## Token

`[sources.github] token`, or the environment variable `PINAKES_GITHUB_TOKEN` (also
`PINAKES__SOURCES__GITHUB__TOKEN`, which wins over the config file, which wins over the short
name). Use a fine-grained
personal access token for public repositories, read-only, with no permissions. It is a secret:
it is sent only to `api.github.com`, as `Authorization: Bearer …`, never through a redirect, and
never appears in logs, errors, metrics, `Debug` output or cards.

Only public repositories get cards. If the token can read private repositories, a record GitHub
marks `private` (or with any `visibility` but `public`) gives no card and no key: the link is
`missing`, and the record leaves the in-memory discovery
cache at once instead of staying for its hour.

GitHub allows 5,000 requests an hour with a token; Pinakes keeps its default host pace (1 a
second), below that. A token GitHub refuses (401: expired, revoked or mistyped) turns the source
off until the daemon restarts: GitHub links get web cards again, one warning is logged, and
`pinakes_source_token_refusals_total{source="github"}` counts each refusal. A lookup never fails
on it.

## Links

- a **repository**: `https://github.com/{owner}/{repo}` (`.git` and a trailing slash allowed;
  `www.github.com` too).

GitHub's own pages (`/orgs`, `/topics`, `/search`, …), users, issues, pull requests, files and
gists are not read; they get web cards.

## What it reads

`GET https://api.github.com/repos/{owner}/{repo}` (about 5 KB). A path GitHub has no
repository for (404: a deleted repository, or another page of the site shaped like one) gets a
web card, which is `missing` when the page is gone too. A renamed one answers a redirect, which is not followed with the
token: its link gets a web card, which follows it.

GitHub's rate limit retries later: a 429, or a 403 whose message names a rate limit. Any other
403 (GitHub blocks access to the repository) gets a web card. A timeout or a server error (408,
5xx) retries later too.

## Card

`SoftwareSourceCode`, named by its full name (`BurntSushi/ripgrep`), described by its description,
with its owner's avatar as the image. The site is `GitHub`.

| Fact | Meaning |
| --- | --- |
| `stars`, `forks`, `topics` | As GitHub counts and lists them |
| `language`, `license` | Its main language; its licence's SPDX ID |
| `default_branch`, `updated` | Its default branch; its last push |
| `archived` | `true` for an archived repository |
| `fork_of` | For a fork: its parent's full name |
| `page_url` | Its page on github.com |

## Examples

- `https://github.com/BurntSushi/ripgrep`
