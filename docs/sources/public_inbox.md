# Mailing lists (public-inbox)

Pinakes reads messages in public-inbox mailing-list archives: gnusha.org (bitcoindev and the old
bitcoin-dev), lore.kernel.org, inbox.sourceware.org, public-inbox.org and any other public-inbox
site. Each message is read from its own mbox (`/raw`), never from its page. No key is needed.
Messages are social content.

| | |
| --- | --- |
| Source | `public_inbox`, official source |
| Links | `https://{host}/{list path}/{message-id}/` and its `/T/`, `/t/` and `/raw` forms; old `lists.linuxfoundation.org/pipermail/…` addresses |
| Fetches | `GET https://{host}/{list path}/{message-id}/raw`; for a pipermail address, first `GET https://gnusha.org/url/https:/lists.linuxfoundation.org/…` |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`), per archive |
| Refresh | Within the hour (social content) |
| Card types | `EmailMessage` |
| Keys | None (facts only) |
| Social content | Yes: 200-character excerpt, purged when deleted ([social content](../design.md#social-content)) |
| Credit | None |
| Switch | `[sources.public_inbox] enabled`, on by default |

## Links

### Messages

A message's root is its page in the archive, `https://{host}/{list path}/{message-id}/`. These
links are [aliases](../api.md#aliases) of it:

- the message in its thread, with `/T/` or `/t/` after the Message-ID;
- the mbox, with `/raw` after the Message-ID;
- the page without the trailing slash.

The list path is one to four path segments of letters, digits, `-`, `_` and `.`, such as
`/pi/bitcoindev` or `/git`. Only `https` links without a port are read.

On gnusha.org, lore.kernel.org, inbox.sourceware.org and public-inbox.org, these aliases apply
from the link alone. On any other host, a link becomes an alias of its root after its first
fetch.

### Other archives

A path segment shaped like a Message-ID (`local@domain`) is read as a message on any host. On a
host not yet known as an archive, the part after the `@` must end in a label of letters, such as
`abc@example.org`. That leaves out package versions such as `gin@v1.9.1` or `react@18`, and
profile links such as `https://medium.com/@someone/a-post-123`.

Each host is remembered for a day by how it answered:

- A host whose `/raw` answered an mbox is an archive. Any Message-ID is read there, and a 404 is a
  missing message.
- A host not known as an archive whose `/raw` answered anything else, a 404 included, is not an
  archive. Its links get ordinary web cards.
- On a host not known as an archive, a `/raw` that cannot be read at all gives the link an ordinary
  web card. A rate limit or a server error is retried instead.

### Pipermail addresses

An old pipermail address of `lists.linuxfoundation.org`,
`https://lists.linuxfoundation.org/pipermail/{list}/{month}/{number}.html`, is read through
gnusha.org, which maps it to the message it archived. The address becomes an alias of that message
after its first fetch. An address gnusha.org maps elsewhere (lightning-dev goes to a plain-text
archive) gets an ordinary web card.

### One Message-ID in two archives

A message is keyed on its Message-ID within its archive. Copies of one message in two archives
stay two resources: anyone can send a message with someone else's Message-ID to another list. The
Message-ID is recorded as the fact `message_id`, but it merges nothing across archives.

## What it fetches

Each read makes one request for the message's mbox:

```text
GET https://{host}/{list path}/{message-id}/raw
```

Pinakes reads the headers and the first `text/plain` part. It decodes quoted-printable, base64 and
encoded-word headers. An answer counts as an mbox when its content type names `mbox`, or when it is
`text/plain` and starts with a `From ` line (lore.kernel.org serves it that way).

A pipermail address first costs one more request, to
`https://gnusha.org/url/https:/lists.linuxfoundation.org/{path}`, which redirects to the message.

The answers mean:

- A 404 from an archive: the card is `missing`.
- An mbox larger than the 1 MiB body limit (a message with attachments): the link gets an ordinary
  web card.
- An mbox for another Message-ID, or one without a `Message-ID` header: an error, and the read is
  retried.

## Card

The card is an `EmailMessage`, named by its subject. `site` is the archive's host.

`description` is the opening words of the message's own text, at most 200 characters. Pinakes
leaves out:

- quoted lines (`>`) and attribution lines ending in "wrote:" or "writes:";
- in-body headers at the start (`From:`, `Date:`, `Subject:`, `To:`, `Cc:`);
- trailers that hold an address, such as `Signed-off-by:`, `Cc:` or `Link:`;
- everything from the signature line (`-- `) or a list footer ("You received this message
  because…") on.

No e-mail address is published. The sender appears by display name only. An address in the subject
or the text is removed.

| Fact | Meaning |
| --- | --- |
| `author` | The sender's display name, from the `From` header |
| `published` | The `Date` header, as RFC 3339 |
| `list` | The list, from `List-Id`, for example `bitcoindev.googlegroups.com` |
| `message_id` | The Message-ID, without angle brackets |
| `in_reply_to` | The Message-ID the message answers |
| `page_url` | The message's page in the archive |

## Keys and groups

Messages have no keys and join no [groups](../api.md#groups). `message_id` and `in_reply_to` are
facts only.

## Credit

The card credits nothing.

## Turn it off

```toml
[sources.public_inbox]
enabled = false
```

Archive links then get ordinary web cards.

## Examples

- `https://gnusha.org/pi/bitcoindev/atCSAVprBE-tAnN3@erisian.com.au/`
- `https://lore.kernel.org/git/pull.2458.git.git.1792437798463.gitgitgadget@gmail.com/`
