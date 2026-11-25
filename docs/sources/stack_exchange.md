# Stack Exchange

Pinakes reads questions on the Stack Exchange network (Stack Overflow, Super User, Server Fault,
Ask Ubuntu, Stack Apps, MathOverflow, every `*.stackexchange.com` site, the language editions of
Stack Overflow, and their metas) from the Stack Exchange API 2.3, by question ID. No key is
needed. An optional app key raises the API's daily quota from 300 requests to 10,000. Questions
are social content.

| | |
| --- | --- |
| Source | `stack_exchange`, official source, with an also-on half |
| Links | `https://{site}/questions/{id}[/{slug}]`, `https://{site}/q/{id}` on every Stack Exchange site |
| Fetches | `GET https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=…` |
| Credentials | Optional `[sources.stack_exchange] key`: an app key that raises the quota to 10,000 requests a day |
| Rate | One request every 288 seconds on `api.stackexchange.com` (300 a day); with the key, one every 8.64 seconds (10,000 a day) |
| Refresh | Within the hour (social content), as far as the quota allows |
| Card types | `Question` |
| Keys | `stackexchange:{site}/{id}`, from the also-on half |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | The site and the licence of the question's text, in `attribution` |
| Switch | `[sources.stack_exchange] enabled`, on by default; the also-on half `[also_on.sources.stack_exchange] enabled`, on by default |

## Links

Every address of one question is one resource. The root is `https://{site}/questions/{id}`, and
these links are [aliases](../api.md#aliases) of it:

- the address with its slug, `https://{site}/questions/{id}/{slug}`;
- the short link, `https://{site}/q/{id}` or `https://{site}/q/{id}/{user}`;
- `www.` spellings of the host;
- a site meta's older host, such as `meta.bitcoin.stackexchange.com` for
  `bitcoin.meta.stackexchange.com`.

A question ID is a positive decimal number of at most 12 digits. These sites are read:

- `stackoverflow.com` and `meta.stackoverflow.com`;
- the language editions `pt.`, `ru.`, `es.` and `ja.stackoverflow.com`, and their metas
  (`pt.meta.stackoverflow.com`, …), and `agents.meta.stackoverflow.com`;
- `superuser.com`, `serverfault.com`, `askubuntu.com`, their metas (`meta.superuser.com`, …), and
  `stackapps.com`;
- `mathoverflow.net` and `meta.mathoverflow.net`;
- every `{site}.stackexchange.com`, its meta `{site}.meta.stackexchange.com`, and Meta Stack
  Exchange itself, `meta.stackexchange.com`.

Together these cover every site the API's `/sites` list names (the module's tests check each one
against a recorded copy). Pinakes asks the API with the site's API name, such as `stackoverflow`,
`bitcoin`, `bitcoin.meta`, `meta` or `pt.stackoverflow`.

Not read here: answers (`/a/{id}`), tag pages, user pages, `api.stackexchange.com` itself, and
hosts with more labels than these shapes, such as `a.b.c.stackexchange.com` or
`meta.meta.stackexchange.com`. Those get web cards.

## What it fetches

Each read makes one request:

```text
GET https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=!)4meWwcO75JCcyGuJACpWc-xLmMU
```

The filter adds the question's body and closing details to the default fields. An answer whose
`items` list is empty (no such question, or a deleted one) is `missing`. An answer without
`items`, or for another question, is an error, and the read is retried.

### Quota and pauses

Without a key the API allows one address 300 requests a day. That quota covers cards, refreshes
and the also-on half together, and the host is paced to it. With an app key, sent as the query's
`key=`, the quota is 10,000 a day. The key identifies an app and is not a secret, but Pinakes keeps
it out of logs and errors, as it does a token. You can register an app on
[Stack Apps](https://stackapps.com/apps/oauth/register) to get one. Set it as
`[sources.stack_exchange] key` or `PINAKES__SOURCES__STACK_EXCHANGE__KEY`.

Requests to the API take turns: each one is made only after the answer before it, and after the
pause that answer asks for. A pause holds for every question on every site. A read that comes
during a pause waits and tries again when it ends. These answers set a pause, of at most a day:

| Answer | Pause |
| --- | --- |
| A `backoff` field, on any answer | The seconds it names |
| `quota_remaining` is 0 | At least an hour |
| The quota error (`error_id` 502) | The wait it names ("more requests available in N seconds"), else an hour |
| "Too many requests" (`error_id` 503) | A minute |
| None of these, with HTTP 429 or 503 | Its `Retry-After`, held between a minute and an hour; without one, 5 minutes for a 429 and a minute for a 503 |

Pinakes reads the first 64 KiB of an error answer's JSON to find these. An API error 500 is also
retried after its pause. Any other API error, such as `bad_parameter` for a site that does not
exist, is a failure: the read is retried on the usual schedule, never sooner.

The quota bounds hourly refreshes. Without a key it keeps about a dozen questions current; with a
key, about 400. Past that, refreshes wait their turn, and so does a deleted question's removal.

## Card

The card is a `Question` ([questions](../api.md#questions)), named by the question's title.
`description` is the start of the question's text: at most 200 characters of plain text, with an
ellipsis when cut. HTML entities in the title and the asker's name are decoded.

`attribution` names the site and the licence of the question's text, for example
"stackoverflow.com, by its authors (CC BY-SA 4.0)". When the API names no licence, it says
"CC BY-SA".

| Fact | Meaning |
| --- | --- |
| `score` | The question's votes |
| `answer_count` | The number of answers |
| `accepted_answer` | The answer its asker accepted, `https://{site}/a/{id}` |
| `tags` | The question's tags, for example `python; string` |
| `author` | The asker's display name |
| `published` | When the question was asked, RFC 3339 |
| `closed_reason` | Why the question was closed, such as `Duplicate` |
| `duplicate_of` | For a duplicate, the questions it repeats, as `https://{site}/questions/{id}` links (see below) |

`duplicate_of` holds at most 8 links, joined with `; `. It keeps the first ones whose whole links
fit in 256 bytes, so a link is never cut.

## Keys and groups

The official source emits no key itself. The also-on half below records the question's key,
`stackexchange:{site}/{id}`, which joins [groups](../api.md#groups). For example,
`key=stackexchange:stackoverflow/509211` finds that question and its duplicates.

## Also on

`stack_exchange` runs on every card this source read. It records one key namespace,
`stackexchange`, whose value is the site's API name and the question ID, such as
`stackexchange:stackoverflow/11227809`. A group holds at most one value of this namespace.

- A question that is not a duplicate carries its own key.
- A question closed as a duplicate of one question carries its original's key instead of its own,
  so the duplicate and its original are one group.
- A chain of duplicates carries the key of its end. When A is a duplicate of B, and B of C, the
  also-on half asks the API for B and then for C, at most three questions on the same site. A, B
  and C are then one group. A loop, or a question that repeats several others, ends the chain.
- A question closed as a duplicate of several questions carries each of their keys, and so joins
  no group.

Only originals on the question's own site count. The requests of the chain share the source's
quota and pauses. The rows live 30 days and expire after twice that.

## Credit

The `attribution` line credits the site and the question's authors, with the licence the API
states for the question's text (CC BY-SA, by version).

## Turn it off

```toml
[sources.stack_exchange]
enabled = false

[also_on.sources.stack_exchange]
enabled = false
```

With the source off, Stack Exchange links get ordinary web cards. With the also-on half off, its
stored keys stop showing and the questions leave their groups.

## Examples

- `https://stackoverflow.com/questions/11227809/why-is-conditional-processing-of-a-sorted-array-faster-than-of-an-unsorted-array`
- `https://bitcoin.stackexchange.com/questions/336`
