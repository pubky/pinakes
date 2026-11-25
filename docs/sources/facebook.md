# Facebook

Pinakes reads Facebook pages from their Open Graph tags. Posts and videos are shown to accounts
only, so their cards come from the link alone and nothing is fetched. No key or account is needed.
Pages and posts are social content.

| | |
| --- | --- |
| Source | `facebook`, official source |
| Links | `facebook.com/{name}`, `facebook.com/{name}/posts/{id}`, `facebook.com/watch/?v={id}`, `facebook.com/reel/{id}`; the same on `fb.com` |
| Fetches | `GET https://www.facebook.com/{name}` for a page; nothing for a post or video |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) on `www.facebook.com` |
| Refresh | Pages and posts: within the hour (social content). Videos: daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Organization` (page), `SocialMediaPosting` (post), `VideoObject` (video) |
| Keys | None (facts only) |
| Social content | Pages and posts: 200-character excerpt of a page's about text; a page is purged when deleted ([social content](../design.md#social-content)). Videos: no |
| Credit | None |
| Switch | `[sources.facebook] enabled`, on by default |

## Links

The roots are:

- a page: `https://www.facebook.com/{name}`;
- a post: `https://www.facebook.com/{name}/posts/{id}`;
- a video: `https://www.facebook.com/watch/?v={id}`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `facebook.com`, `m.facebook.com`, `web.facebook.com`,
  `mbasic.facebook.com`, `fb.com` and `www.fb.com`, or with `http`;
- a reel's address, `https://www.facebook.com/reel/{id}`, an alias of the video;
- a name with capitals (roots are lower case), an address with a trailing slash or other query
  parameters.

A name is 2 to 64 letters, digits, dots and underscores, so a numeric page ID is a name too. Post
and video IDs are numbers of at most 20 digits.

Not read here: groups, photos, share links, `profile.php` links and Facebook's own pages (such as
`/login`, `/about` and `/marketplace`). Event links (`https://www.facebook.com/events/{id}`) belong
to the event route, whose `facebook` adapter reads them.

## What it fetches

A page's read makes one request, `GET https://www.facebook.com/{name}`. A 404 or 410 is `missing`.
Any status that is neither listed here nor a refusal is an error, and the read is retried. A 429
waits at least the time its `Retry-After` header names (up to an hour), or 5 minutes when it names
none. A 408 or a server error waits at least a minute.

A post or video makes no request.

### Refused requests

Facebook refuses a request with a sign-in redirect, a challenge page, or a 401, 403 or 451. A page
that does not exist redirects to the sign-in page too, so it reads as refused and not as `missing`.

A page never read before then gets the card of its shape alone, an `Organization` named "Profile on
facebook.com", with the description "Blocked by the site." and the `blocked` fact. The link still
resolves, groups and counts. A refusal on a refresh keeps the card read before. After a refusal the
link is asked again after a day, then after 2, 4 and 7 more days, and every 7 days while one refusal
follows another ([refusals](../operations.md#failures-and-retries)).

## Card

### Pages

A page is an `Organization`.

- `name` is the page's title.
- `description` is the page's about text, in the page's language: the first 200 characters after
  its counts (`N followers · N talking about this.`), without e-mail addresses.
- `image` is the page's picture.

The card takes these only from the page itself: the address the page gives itself (`og:url`, else
the address the fetch ended at) must name the link's page. Dots and case in the name do not count.

Any other page, or one without tags (a consent page or Facebook's generic page), does not say that
the page is gone. It gives the plain card, "Profile on facebook.com" with the `handle` fact. A
stored card that says more stays until a second read, an hour later by default
(`limits.missing_ttl_secs`, at least an hour), answers the same; then the plain card replaces it.

### Posts and videos

A post is a `SocialMediaPosting` named "Post on facebook.com", with its page as `author`. A video
is a `VideoObject` named "Video on facebook.com". Neither has text or an image.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the name of its page, as the link spells it, in lower case |
| `handle` | For a page: its name, as the link spells it, in lower case |
| `blocked` | `true` when Facebook refused the request |

## Keys and groups

Facebook cards have no keys. Names and IDs join nothing.

## Credit

The card credits nothing. A post's page is in `author`.

## Turn it off

```toml
[sources.facebook]
enabled = false
```

Facebook page, post and video links then get ordinary web cards. Event links still go to the event
route.

## Examples

- `https://www.facebook.com/facebook` (a page)
