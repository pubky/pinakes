# Docker Hub

Pinakes reads container image repositories on Docker Hub from its v2 API, by namespace and name. No
key is needed.

| | |
| --- | --- |
| Source | `docker_hub`, official source |
| Links | Repository pages and v2 API addresses on `hub.docker.com`; `docker:{namespace}/{name}` roots |
| Fetches | `GET https://hub.docker.com/v2/repositories/{namespace}/{name}/` |
| Credentials | None |
| Rate | At most 3 requests per second on `hub.docker.com` (the API allows 180 anonymous requests a minute). A source's rate only lowers the default, so with the default `limits.host_requests_per_second` (1) the host gets 1 |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `SoftwareApplication` |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.docker_hub] enabled`, on by default |

## Links

Every address of one repository is one resource. The root is `docker:{namespace}/{name}`, in lower
case. Official images are in the `library` namespace (`docker:library/nginx`). These links are
[aliases](../api.md#aliases) of the root:

- `https://hub.docker.com/_/{name}` (an official image) and its `/tags` tab;
- `https://hub.docker.com/r/{namespace}/{name}` and its `/tags` tab, including `/r/library/{name}`;
- `https://hub.docker.com/repository/docker/{namespace}/{name}` (the owner's view) and its tabs
  `/general`, `/tags`, `/builds`, `/collaborators`, `/webhooks` and `/settings`;
- `https://hub.docker.com/v2/repositories/{namespace}/{name}/`;
- `https://hub.docker.com/v2/namespaces/{namespace}/repositories/{name}`.

A namespace or name in a link can be in any case. Each is letters, digits, `.`, `_` and `-`, and
starts and ends with a letter or digit. A `docker:` root that is not in this form, or not in lower
case, is `missing` at once and never fetched.

Not read here: other tabs, a single tag or layer, user pages, searches and other registries such as
`ghcr.io`.

## What it fetches

One request per read: `GET https://hub.docker.com/v2/repositories/{namespace}/{name}/`. A repository
the API does not know (404) is `missing`. A private repository answers the same. An answer for
another repository, or any other status, is an error and the read is retried.

## Card

The type is `SoftwareApplication`.

- `site` is "Docker Hub".
- `name` is the image name: `nginx` for an official image, else `{namespace}/{name}`.
- `description` is the repository's short description.

| Fact | Meaning |
| --- | --- |
| `registered` | When the repository was registered (RFC 3339) |
| `pull_count` | Pulls so far |
| `star_count` | Stars |
| `last_updated` | When the repository was last updated, as the API gives it (RFC 3339) |
| `official` | `true` for an official image (`library`), else absent |
| `status` | The repository's status, for example `active` |
| `archived` | `true` when the API says the repository is archived, else absent |
| `categories` | Docker Hub's categories |
| `page_url` | `https://hub.docker.com/_/{name}` or `https://hub.docker.com/r/{namespace}/{name}` |

## Keys and groups

The source emits no keys. The links of one repository join as aliases of its root.

## Credit

The card shows no credit line.

## Turn it off

```toml
[sources.docker_hub]
enabled = false
```

Docker Hub links then get ordinary web cards, and `docker:` roots wait until the source is on again.

## Examples

- `https://hub.docker.com/_/nginx`
- `https://hub.docker.com/r/btcpayserver/btcpayserver`
- `https://hub.docker.com/r/lncm/bitcoind/tags`
