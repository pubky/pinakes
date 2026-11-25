# Instagram

Pinakes reads Instagram posts, reels and profiles from their pages' Open Graph tags, when Instagram
writes them for a request without an account. No key or account is needed: Instagram's oEmbed
endpoint needs an app token and is not used. Instagram refuses many requests, and such a link gets
a plain card. Posts, reels and profiles are social content.

| | |
| --- | --- |
| Source | `instagram`, official source |
| Links | `instagram.com/{name}/`, `instagram.com/p/{code}/`, `instagram.com/reel/{code}/`, `instagram.com/tv/{code}/`; the same on `instagr.am` |
| Fetches | `GET` of the root: `https://www.instagram.com/{name}/`, `/p/{code}/` or `/reel/{code}/` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.instagram.com` |
| Refresh | Every day (social content, read less often than other sites': Instagram refuses an address that asks often). A link Instagram refuses: after 1 day, doubling to every 7 days ([refusals](../operations.md#failures-and-retries)) |
| Card types | `SocialMediaPosting` (post), `VideoObject` (reel), `Person` (profile) |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when Instagram answers 404 or 410 ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.instagram] enabled`, on by default |

## Links

The roots are:

- a post: `https://www.instagram.com/p/{code}/`;
- a reel: `https://www.instagram.com/reel/{code}/`;
- a profile: `https://www.instagram.com/{name}/`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `instagram.com`, `instagr.am` and `www.instagr.am`, or with `http`;
- `/tv/{code}/`, an alias of the reel;
- a post or reel address with its author's name first: `/{name}/p/{code}/`, `/{name}/reel/{code}/`;
- a name with capitals (roots are lower case), an address without the trailing slash, or one with
  query parameters.

A name is at most 30 letters, digits, `_`, `.` and `-`. A code is 6 to 40 letters, digits, `_` and
`-`.

Instagram serves a reel under both `/p/{code}/` and `/reel/{code}/`, with the same code. The link
alone cannot say whether a `/p/` code is a reel, so this source keeps the two apart: one reel linked
both ways is two cards.

Not read here: stories, `/explore/` pages, sign-in pages, a profile's sub-pages (such as
`/{name}/reels/`) and Instagram's other own pages.

## What it fetches

One request per read, for the root's page. A 404 or 410 is `missing`. Any status that is neither
listed here nor a refusal is an error, and the read is retried. A 429 waits at least the time its
`Retry-After` header names (up to an hour), or 5 minutes when it names none. A 408 or a server
error waits at least a minute.

Instagram answers a removed or private post, and a reel that does not exist, with status 200 and a
page without the object's tags. That answer gives the plain card (below), not `missing`.

### Refused requests

Instagram refuses a request with a sign-in redirect, a challenge page, or a 401, 403 or 451. A
profile's page usually answers with the sign-in redirect.

A link never read before then gets the card of its shape alone ("Post on instagram.com", "Video on
instagram.com" or "Profile on instagram.com"), with the description "Blocked by the site." and the
`blocked` fact. The link still resolves, groups and counts. A refusal on a refresh keeps the card
read before. After a refusal the link is asked again after a day, then after 2, 4 and 7 more days,
and every 7 days while one refusal follows another
([refusals](../operations.md#failures-and-retries)).

A sign-in redirect whose address carries `is_from_rle` (Instagram's rate-limit enforcement) refuses
every page to the address Pinakes asks from, not only this one. It counts as a refusal at once,
without asking for the sign-in page, and no request goes to Instagram (`www.instagram.com`,
`instagram.com`) for a day, then for 2, 4 and 7 days after each such refusal in a row; a request
already waiting for its turn sends nothing either. Once the pause is over, a final answer from the
host that refused ends the streak (a redirect from `instagram.com` to `www.instagram.com` is no
answer yet, and neither is a page of another Instagram host such as `about.instagram.com`). Links
due meanwhile wait, and a new link is read once the pause is over. The pause is kept in memory: a
restart forgets it, while each link's own wait is stored.

## Card

### Posts and reels

A post is a `SocialMediaPosting`, a reel a `VideoObject`. The page's title gives both the author
and the caption (`Name on Instagram: "caption"`).

- `name` is the first 120 characters of the caption on one line, or "Post by {author}" without a
  caption.
- `description` is the first 200 characters of the caption, with its line breaks; without a
  caption, of the page's description.
- `image` is the page's image.
- `embed` is Instagram's own frame for the post, from the link alone ([embed](../api.md#embed)):
  `https://www.instagram.com/p/{code}/embed/captioned/` for a post,
  `https://www.instagram.com/reel/{code}/embed/captioned/` for a reel (one read from `/tv/{code}/`
  too). Its `kind` is `post`, without `height` or `aspect`: the frame sets its own height. The card
  has it whether the page was read, answered without the post's tags, or refused.

The card takes its name, description and image only from the object's own page: the address the
page gives itself (`og:url`, else the address the fetch ended at) must hold the link's code.

Any other page, or one without these tags, gives the plain card: "Post on instagram.com" or "Video
on instagram.com". A stored card that says more stays until a second read, an hour later by default
(`limits.missing_ttl_secs`, at least an hour), answers the same; then the plain card replaces it.
The change history stays, because the object is not known to be gone.

### Profiles

A profile is a `Person` named by its display name. `description` is at most 200 characters of the
page's description after its counts, without e-mail addresses. `image` is the picture. A profile
has no `embed`.

The page's title must name the link's name (`Name (@name) • Instagram photos and videos`).
Otherwise the card is the plain one, "Profile on instagram.com" with the `handle` fact, and it
replaces a stored card at once.

### Facts

| Fact | Meaning |
| --- | --- |
| `author_name` | For a post or reel: the author, as the page's title names them |
| `handle` | For a profile: its name, in lower case |
| `blocked` | `true` when Instagram refused the request |

## Keys and groups

Instagram cards have no keys. Names and codes join nothing.

## Credit

The card credits nothing. A post's or reel's author is in `author_name`.

## Turn it off

```toml
[sources.instagram]
enabled = false
```

Instagram links then get ordinary web cards. A post's or reel's `embed` stays, because it comes from
the link alone.

## Examples

- `https://www.instagram.com/p/C2vXx0yPvYq/`
- `https://www.instagram.com/instagram/` (a profile)
