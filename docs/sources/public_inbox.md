# Mailing lists (public-inbox)

Messages in public-inbox archives: gnusha.org (bitcoindev and the old bitcoin-dev), lore.kernel.org,
inbox.sourceware.org, public-inbox.org and any other public-inbox site. Each message is read from
its own mbox (`/raw`), never from its page. No key. Messages are social content: the card quotes
at most 200 characters, refreshes within the hour and is removed once the message is gone.

## Links

- a **message**: `https://{host}/{list path}/{message-id}/` (the root), with `/T/` or `/t/` (the
  message in its thread), `/raw` and without the trailing slash as aliases;
- an old **pipermail** address of `lists.linuxfoundation.org`
  (`/pipermail/{list}/{month}/{number}.html`): gnusha.org maps it to the message it archived, and
  the address becomes an alias of that message after its first fetch. An address gnusha.org maps
  elsewhere (lightning-dev goes to a plain-text archive) gets an ordinary web card.

A message is keyed on its Message-ID within its archive. Copies of one message in two archives
stay two resources: anyone can send a message with someone else's Message-ID to another list,
so the Message-ID is recorded (`message_id`) but merges nothing across archives.

On gnusha.org, lore.kernel.org, inbox.sourceware.org and public-inbox.org the aliases apply from
the link alone. A path segment shaped like a Message-ID on any other host is read as a message
too: a host whose `/raw` does not answer as an mbox is remembered for a day, and its links get
ordinary web cards.

## What it reads

`GET https://{host}/{list path}/{message-id}/raw` (`application/mbox`, usually 10–30 KB): the
headers and the first `text/plain` part (quoted-printable, base64 and encoded-word headers are
decoded). A pipermail address is first sent to `https://gnusha.org/url/https:/lists.linuxfoundation.org/…`,
which redirects to the message. A message the archive does not have is `missing`; one larger than
the 1 MB body limit (attachments) gets an ordinary web card.

## Card

`EmailMessage`, named by its subject and described by the opening words of its own text (quoted
lines, "… wrote:" lines, the signature and list footers left out). No address is published: the
sender appears by display name only.

| Fact | Meaning |
| --- | --- |
| `author` | The sender's display name |
| `published` | Its `Date` header |
| `list` | Its list (`List-Id`), e.g. `bitcoindev.googlegroups.com` |
| `message_id` | Its Message-ID, without angle brackets |
| `in_reply_to` | The Message-ID it answers |
| `page_url` | Its page in the archive |

## Examples

- `https://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au/`
- `https://lore.kernel.org/git/pull.2458.git.git.1792437798463.gitgitgadget@gmail.com/`
