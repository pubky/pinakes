# Stack Exchange

Questions on the Stack Exchange network (Stack Overflow, Super User, Server Fault, Ask Ubuntu,
Stack Apps, MathOverflow, every `*.stackexchange.com` site, the language editions of Stack
Overflow, and their metas), read from the Stack Exchange API 2.3. No key: the API allows an
address 300 requests a day without one; a used-up quota (or a `backoff` the API asks for) makes a
question wait and try again later.

## Links

`https://{site}/questions/{id}` is the root; the address with its slug, the short link
`/q/{id}[/{user}]` and `www.` spellings are aliases of it. Answers (`/a/{id}`), tags and users are
read as web pages.

## What it reads

`GET https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=…`, one request per
question (the filter adds the question's body and closing details to the default fields). A
question the API does not return (none, or deleted) is `missing`.

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
| `duplicate_of` | For a duplicate, the question it repeats (several joined with `; `) |

## Also on

`stack_exchange`: a question's id on its site is the key `stackexchange:{site}/{id}` (its API
name: `stackoverflow`, `bitcoin`, `meta.bitcoin`, `pt.stackoverflow`). A question closed as a
duplicate carries its original's key instead of its own, so the duplicate and its original are
one group, a hint; one closed as a duplicate of several questions carries each of their keys and
joins none. `key=stackexchange:stackoverflow/509211` finds the original and its duplicates.

## Examples

- `https://stackoverflow.com/questions/11227809/why-is-conditional-processing-of-a-sorted-array-faster-than-of-an-unsorted-array`
- `https://bitcoin.stackexchange.com/questions/336`
