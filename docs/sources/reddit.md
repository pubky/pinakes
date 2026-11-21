# Reddit

Reddit posts, subreddits and users, from Reddit's public JSON: the `.json` form of any page.
No key and no account; ten requests a minute, Reddit's limit for requests without an account.
Posts are social content: the card quotes at most 200 characters, refreshes within the hour
and is removed once the post is gone.

## Links

A post is named by its id, a subreddit and a user by their names:
`https://www.reddit.com/comments/{id}/`, `https://www.reddit.com/r/{name}/` and
`https://www.reddit.com/user/{name}/` are the roots. Reddit's other hosts (old., new., np.,
m., none), a post's subreddit and slug (`/r/{sub}/comments/{id}/{slug}/`), `redd.it/{id}`,
`/u/{name}` and a capitalised name are aliases of them. Share links (`/r/{sub}/s/{code}`) name
nothing without a fetch and are not read. A comment's permalink
(`…/comments/{id}/{slug}/{comment}/`) and a post's link that selects one comment
(`…/comments/{id}/?comment={comment}`) are not read either: a comment is its own thing, never its
post.

## What it reads

- A post: `GET https://www.reddit.com/comments/{id}.json?raw_json=1`, the listing whose first
  child is the post.
- A subreddit: `GET https://www.reddit.com/r/{name}/about.json?raw_json=1`.
- A user: `GET https://www.reddit.com/user/{name}/about.json?raw_json=1`.

A 404 is `missing`, and so is the stub Reddit answers for a deleted or removed post: a removal it
records (`removed_by_category`), or the text replaced by `[deleted]` or `[removed]`. A deleted
account's post stays up under `[deleted]`, and its card names no author. An answer for another
object is an error. Reddit refuses many requests without a browser with its network-security page
(403): the card of the link's shape alone, "Post on reddit.com" or "Profile on reddit.com", with the
`blocked` fact. The link still resolves, groups and counts. A refusal on a refresh keeps the card read before, as a wall on a web page does. A refused post keeps the post's type, so
it stays social content: once the post is gone, its card keeps nothing of the post (no name, text,
image or fact, such as its subreddit or author, not even its type: only its link and status, with
the type every gone post has, [`SocialMediaPosting`](../api.md#card)), its change history
and what the also-on lookups found for it are deleted, and the spellings of the post saved before
they became its aliases, whenever they become one, keep only the line naming the post.

The recorded fixtures hold Reddit's refusals; the JSON fixtures are written by hand after
Reddit's public JSON, as their provenance says.

## Card

A post is a `DiscussionForumPosting` named by its title, with its text (else its title) and
its thumbnail (none for an over-18 post). A subreddit is an `Organization` named `r/{name}`,
with its public description and icon. A user is a `Person` named `u/{name}`, with their
profile description and avatar.

| Fact | Meaning |
| --- | --- |
| `author`, `subreddit` | A post's author and subreddit |
| `score`, `comments` | A post's score and comment count |
| `published` | RFC 3339: a post's time, a subreddit's or user's creation |
| `handle` | A subreddit's or user's name |
| `subscribers` | A subreddit's |
| `karma` | A user's total |
| `blocked` | `true` when Reddit refused the request |

## Examples

- `https://www.reddit.com/r/announcements/comments/1k3d0pk/`
- `https://www.reddit.com/r/announcements/` (a subreddit)
- `https://www.reddit.com/user/spez/` (a user)
