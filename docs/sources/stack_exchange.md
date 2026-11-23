# Stack Exchange

Questions on the Stack Exchange network (Stack Overflow, Super User, Server Fault, Ask Ubuntu,
Stack Apps, MathOverflow, every `*.stackexchange.com` site, the language editions of Stack
Overflow, and their metas), read from the Stack Exchange API 2.3. Without a key the API allows an
address 300 requests a day, for cards, refreshes and the also-on half together; the host is paced
to that quota (one request every 288 seconds). With an app key (`[sources.stack_exchange] key`,
or `PINAKES__SOURCES__STACK_EXCHANGE__KEY`), sent as the query's `key=`, the quota is 10,000 a
day and the host is paced to that (one request every 8.64 seconds). The key identifies an app and
is not a secret; it is still kept out of logs, errors and `/v1/info`, as a token is. Register an
app on [Stack Apps](https://stackapps.com/apps/oauth/register) for one.

Every answer says how much of the day's quota is left (`quota_remaining`). Once it is 0, no
request is made for an hour; the API's quota error then names when requests are allowed again
("more requests available in N seconds"), and no request is made before then. A `backoff` the
API asks for (on any answer, an error's too: the first 64 KiB of an error in JSON are read), its
quota error (the wait it names, else an hour), its "too many requests" (a minute) and, naming
none of these, an HTTP 429 or 503 (its `Retry-After`, a minute at least) pause every request to
it, for every question on every site, until the pause ends: a question asked meanwhile waits and
tries again then. Requests to the API
take turns, so each is made only after the answer before it, and the pause that answer asks for. Any other error of the API (`bad_parameter` for a site there is not, …) is a failure, tried
again on the usual schedule, never sooner.

## Links

`https://{site}/questions/{id}` is the root; the address with its slug, the short link
`/q/{id}[/{user}]`, `www.` spellings and a meta's older host (`meta.bitcoin.stackexchange.com`
for `bitcoin.meta.stackexchange.com`) are aliases of it. Every site the API's `/sites` lists is
read (recorded in the fixtures), its metas too: `bitcoin.meta.stackexchange.com`,
`meta.stackexchange.com`, `meta.stackoverflow.com`, `pt.meta.stackoverflow.com`. Answers (`/a/{id}`), tags and users are
read as web pages.

## What it reads

`GET https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=…`, one request per
question (the filter adds the question's body and closing details to the default fields). A
question the API does not return (none, or deleted) is `missing`. Questions are social content:
the card quotes at most 200 characters of the question, refreshes within the hour and is removed
once the question is gone (a second read confirms it).

## Card

A `Question` ([Questions](../api.md#questions)) named by its title and described by the start
of its text (200 characters). Its text is its authors' under CC BY-SA: `attribution` names the
site and the licence (`stackoverflow.com, by its authors (CC BY-SA 4.0)`), to show with the
card.

| Fact | Meaning |
| --- | --- |
| `score`, `answer_count` | Its votes and answers |
| `accepted_answer` | `https://{site}/a/{id}`, the answer its asker accepted |
| `tags` | E.g. `python; string` |
| `author`, `published` | Its asker and when it was asked (RFC 3339) |
| `closed_reason` | Why it was closed (`Duplicate`, …) |
| `duplicate_of` | For a duplicate, the question it repeats (several joined with `; `: the first ones whose whole links fit 256 bytes, of 8 at most) |

## Also on

`stack_exchange`: a question's id on its site is the key `stackexchange:{site}/{id}` (its API
name: `stackoverflow`, `bitcoin`, `bitcoin.meta`, `meta`, `pt.stackoverflow`). A question
closed as a duplicate carries its original's key instead of its own, so the duplicate and its
original are one group, a hint. A chain (A a duplicate of B, B of C) carries the key of its end:
the lookup asks the API for the original, and for its original in turn, at most three questions
on the same site, so A, B and C are one group. One closed as a duplicate of several questions
carries each of their keys and joins none. `key=stackexchange:stackoverflow/509211` finds the original and its duplicates.

## Examples

- `https://stackoverflow.com/questions/11227809/why-is-conditional-processing-of-a-sorted-array-faster-than-of-an-unsorted-array`
- `https://bitcoin.stackexchange.com/questions/336`
