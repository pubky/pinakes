# Docker Hub

Container image repositories on Docker Hub, read from its v2 API by namespace and name. No key;
the API allows 180 anonymous requests a minute per address (three a second). Docker states no
licence for the data; the descriptions are written by each repository's owner.

## Links

Names are in lower case. `docker:{namespace}/{name}` and every Docker Hub address of a repository
are one resource; official images are in the `library` namespace (`docker:library/nginx`):

- `https://hub.docker.com/_/{name}` (an official image), with any tab (`/tags`, …);
- `https://hub.docker.com/r/{namespace}/{name}`, with any tab, including `/r/library/{name}`;
- `https://hub.docker.com/repository/docker/{namespace}/{name}` (the owner's view);
- `https://hub.docker.com/v2/repositories/{namespace}/{name}/`;
- `https://hub.docker.com/v2/namespaces/{namespace}/repositories/{name}`.

A repository the API does not know (404 "object not found"; private repositories answer the same)
is `missing`.

## What it reads

`GET https://hub.docker.com/v2/repositories/{namespace}/{name}/`.

## Card

`SoftwareApplication`, with `site` "Docker Hub", `name` the image name (`nginx` for an official
image, else `{namespace}/{name}`), `description` the repository's short description and `start`
the date it was registered.

| Fact | Meaning |
| --- | --- |
| `pull_count` | Pulls so far |
| `star_count` | Stars |
| `last_updated` | When an image was last pushed |
| `official` | `true` for an official image (`library`) |
| `status` | The repository's status, e.g. `active` |
| `archived` | `true` when the API says the repository is archived |
| `categories` | Docker Hub's categories |
| `page_url` | `https://hub.docker.com/_/{name}` or `https://hub.docker.com/r/{namespace}/{name}` |

## Examples

- `https://hub.docker.com/_/nginx`
- `https://hub.docker.com/r/btcpayserver/btcpayserver`
- `https://hub.docker.com/r/lncm/bitcoind/tags`
