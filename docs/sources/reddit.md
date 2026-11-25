# Reddit

Pinakes reads Reddit posts, subreddits and users from Reddit's public JSON, the `.json` form of a
page. No key or account is needed. Reddit refuses many requests without a browser, and such a link
gets a plain card. Posts and users are social content.

| | |
| --- | --- |
| Source | `reddit`, official source |
| Links | `reddit.com/r/{sub}/comments/{id}/{slug}/`, `reddit.com/comments/{id}/`, `redd.it/{id}`, `reddit.com/r/{name}/`, `reddit.com/user/{name}/`, `reddit.com/u/{name}` |
| Fetches | `GET https://www.reddit.com/comments/{id}.json?raw_json=1`, `GET https://www.reddit.com/r/{name}/about.json?raw_json=1` or `GET https://www.reddit.com/user/{name}/about.json?raw_json=1` |
| Credentials | None |
| Rate | 0.17 requests per second on `www.reddit.com` (Reddit's limit of 10 a minute without an account) |
| Refresh | Posts and users: within the hour (social content). Subreddits: daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `DiscussionForumPosting` (post), `Organization` (subreddit), `Person` (user) |
| Keys | None (facts only) |
| Social content | Posts and users: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)). Subreddits: no |
| Credit | None |
| Switch | `[sources.reddit] enabled`, on by default |

## Links

The roots are:

- a post: `https://www.reddit.com/comments/{id}/`;
- a subreddit: `https://www.reddit.com/r/{name}/`;
- a user: `https://www.reddit.com/user/{name}/`.

These links are [aliases](../api.md#aliases) of the root:

- the same addresses on `reddit.com`, `old.reddit.com`, `new.reddit.com`, `np.reddit.com` and
  `m.reddit.com`, or with `http`;
- a post's address with its subreddit, its slug or both (`/r/{sub}/comments/{id}/{slug}/`);
- `https://redd.it/{id}` and `https://www.redd.it/{id}`;
- `/u/{name}`, an alias of the user;
- a subreddit or user name with capitals (roots are lower case), an address without the trailing
  slash or with query parameters (such as `?sort=new`).

A post ID is 2 to 12 lower-case letters and digits. A name is 2 to 30 letters, digits, `_` and `-`.

Not read here:

- a comment's permalink (`…/comments/{id}/{slug}/{comment}/`) and a post's link that selects one
  comment (`…/comments/{id}/?comment={comment}`): a comment is its own thing, never its post;
- share links (`/r/{sub}/s/{code}`), which name nothing without a fetch;
- wiki and search pages.

## What it fetches

One request per read, for the object's JSON. A post's answer is a listing whose first entry is the
post.

These answers make a card `missing`:

- a 404;
- a listing with no post, or an `about` answer that names no subreddit or user;
- the stub of a deleted or removed post: a removal Reddit records (`removed_by_category`), or text
  replaced by `[deleted]` or `[removed]`.

An answer for another object, an answer of another shape and any status that is neither listed here
nor a refusal are errors: the read is retried and a stored card stays. A 429 waits at least the
time its `Retry-After` header names (up to an hour), or 5 minutes when it names none. A 408 or a
server error waits at least a minute.

### Refused requests

Reddit refuses a request with its network-security page (403), another challenge, a sign-in redirect
or a 401. A link never read before then gets the card of its shape alone, "Post on reddit.com" or
"Profile on reddit.com" (for a subreddit or user), with the description "Blocked by the site." and
the `blocked` fact. It keeps the type its readable card has, so a refused post is still social
content. The link still resolves, groups and counts. A refusal on a refresh keeps the card read
before. After a refusal the link is asked again a day later, then 2, 4 and 7 days later, and every 7
days while the refusals go on ([refusals](../operations.md#failures-and-retries)).

## Card

### Posts

A post is a `DiscussionForumPosting`.

- `name` is the first 120 characters of the title.
- `description` is the first 200 characters of the text, with its line breaks, else of the title.
- `image` is the thumbnail, else the image the post links; an over-18 post has none.

The post of a deleted account stays up under `[deleted]`, and its card names no author.

A post confirmed gone keeps nothing of itself: no name, text, image or fact (its subreddit and
author included), no change history and nothing the also-on lookups found for it. Its type is the
one every gone post has, `SocialMediaPosting`. Spellings saved before they became its aliases keep
only the line that names the post ([social content](../design.md#social-content)).

### Subreddits

A subreddit is an `Organization` named `r/{name}`. `description` is the first 200 characters of its
public description, else of its title. `image` is its icon, unless the subreddit is over-18.

### Users

A user is a `Person` named `u/{name}`. `description` is the first 200 characters of their profile
description, without e-mail addresses. `image` is their avatar.

### Facts

| Fact | Meaning |
| --- | --- |
| `author` | For a post: the author's name |
| `subreddit` | For a post: its subreddit |
| `score`, `comments` | For a post: its score and number of comments |
| `published` | RFC 3339: when the post, subreddit or account was created |
| `handle` | For a subreddit or user: its name, in lower case |
| `subscribers` | For a subreddit: its number of subscribers |
| `karma` | For a user: their total karma |
| `blocked` | `true` when Reddit refused the request |

## Keys and groups

Reddit cards have no keys. Names and post IDs join nothing.

## Credit

The card credits nothing. A post's author is in `author`.

## Turn it off

```toml
[sources.reddit]
enabled = false
```

Reddit links then get ordinary web cards.

## Examples

- `https://www.reddit.com/r/announcements/comments/1k3d0pk/`
- `https://www.reddit.com/r/announcements/` (a subreddit)
- `https://www.reddit.com/user/spez/` (a user)
