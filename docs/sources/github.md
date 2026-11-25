# GitHub

Pinakes reads public repositories on github.com from GitHub's REST API, with the operator's token.
The source is off without a token: GitHub links then get ordinary web cards.

| | |
| --- | --- |
| Source | `github`, official source |
| Links | `github.com/{owner}/{repo}` (also with `.git`, a trailing slash or `www.`) |
| Fetches | `GET https://api.github.com/repos/{owner}/{repo}` |
| Credentials | Required: `[sources.github] token` or `PINAKES_GITHUB_TOKEN`. Off without it |
| Rate | GitHub's 5,000 requests an hour with a token, about 1.4 a second on `api.github.com`: above the shared default (`limits.host_requests_per_second`), which therefore applies |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareSourceCode` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.github] enabled`, on by default, and on only with a token |

## Token

Set the token as `[sources.github] token`, `PINAKES__SOURCES__GITHUB__TOKEN` or
`PINAKES_GITHUB_TOKEN`. The short variable `PINAKES_GITHUB_TOKEN` fills the token only when the
config file and the `PINAKES__SOURCES__GITHUB__TOKEN` form leave it unset.

```toml
[sources.github]
enabled = true
token = "<fine-grained token>"
```

Use a fine-grained personal access token for public repositories, read-only. The token is a
secret. It is sent only to `api.github.com`, as `Authorization: Bearer …`, and never through a
redirect. It never appears in logs, errors, `Debug` output or cards.

Only public repositories get cards. If the token can read private repositories, a record GitHub
marks `private`, or with any `visibility` other than `public`, gives no card and no key. The link
is `missing`, and the record leaves the in-memory discovery cache at once instead of staying there
for its hour.

### Refused tokens

GitHub refuses a token with a 401, or with a 403 whose `message` says "Bad credentials" (an
expired, revoked or mistyped token). A refusal turns the source off for an hour:

- GitHub links get web cards meanwhile;
- one warning is logged per hour off;
- `pinakes_source_token_refusals_total{source="github"}` counts each refusal.

After the hour, Pinakes sends the token again.

## Links

A repository's link is `https://github.com/{owner}/{repo}`. Pinakes also reads it with `.git`
after the name, with a trailing slash, and on `www.github.com`. The root is the repository's
page as GitHub names it (`html_url`), and every other spelling becomes an
[alias](../api.md#aliases) of it after its first fetch.

An owner is 1 to 39 letters, digits and hyphens, not starting with a hyphen. A repository name is
1 to 100 letters, digits, `-`, `_` and `.`, other than `.` and `..`.

Not read here: GitHub's own pages (`/orgs`, `/topics`, `/search`, `/advisories`, `/solutions` and
the other first path segments GitHub reserves), user and organization pages, issues, pull requests,
files and gists. They get web cards.

## What it fetches

Each read makes one request, through the in-memory discovery cache (an hour):

```text
GET https://api.github.com/repos/{owner}/{repo}
Accept: application/vnd.github+json
X-GitHub-Api-Version: 2022-11-28
```

The answers mean:

| Answer | Outcome |
| --- | --- |
| 200, a public repository | The card |
| 200, a repository that is not public | `missing` |
| 301, 302, 307 or 308 (a renamed or moved repository) | Not followed with the token: the link gets a web card, which follows the redirect |
| 404 (a deleted repository, or another page of the site shaped like one) | The link gets a web card, which is `missing` when the page is gone too |
| 401, or 403 saying "Bad credentials" | The token is refused (see [Refused tokens](#refused-tokens)): a web card |
| 429, or 403 whose `message` names a rate limit or abuse detection | GitHub's rate limit: read again later |
| Any other 403 (GitHub blocks access to the repository) | A web card |
| 408 or 5xx | Read again later |
| Any other status | An error, and the read is retried |

GitHub's rate limit is the token's own quota. It is stored as `quota` and charges the link nothing.
The link is read again when GitHub says: its `Retry-After`, else its `X-RateLimit-Reset`, at least
a minute and at most an hour. A timeout or a server error is stored as `upstream` and waits the
same way.

## Card

The card is a `SoftwareSourceCode`, named by the repository's full name, such as
`BurntSushi/ripgrep`.

- `site` is "GitHub".
- `description` is the repository's description, without e-mail addresses.
- `image` is the owner's avatar.

| Fact | Meaning |
| --- | --- |
| `stars` | The number of stars |
| `forks` | The number of forks |
| `topics` | The repository's topics |
| `language` | Its main language, as GitHub detects it |
| `license` | Its licence's SPDX ID (left out when GitHub says `NOASSERTION`) |
| `default_branch` | Its default branch |
| `updated` | Its last push |
| `archived` | `true` for an archived repository |
| `fork_of` | For a fork, its parent's full name |
| `page_url` | The repository's page on github.com |

## Keys and groups

GitHub repositories have no keys and join no [groups](../api.md#groups). `fork_of` is a fact only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.github]
enabled = false
```

GitHub links then get ordinary web cards, as they do without a token.

## Examples

- `https://github.com/BurntSushi/ripgrep`
