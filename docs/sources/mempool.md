# mempool.space

Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, read from the
mempool.space API by the identifier in the link. When mempool.space can't answer, blocks,
transactions and addresses are read from the Esplora API at blockstream.info instead (same
card). No key. Public blockchain and Lightning gossip data.

## Links

Every mempool.space link of one object is one resource, with a root in the object's own scheme:

| Object | Links | Root |
| --- | --- | --- |
| Block | `https://mempool.space/block/{hash}` | `btc:block/{hash}` |
| Transaction | `https://mempool.space/tx/{txid}` (`#vin=…`/`#vout=…` included) | `btc:tx/{txid}` |
| Address | `https://mempool.space/address/{address}`, `bitcoin:{address}[?…]` | `btc:address/{address}` |
| Lightning node | `https://mempool.space/lightning/node/{public key}` | `ln:node/{public key}` |
| Lightning channel | `https://mempool.space/lightning/channel/{id}`, also by short id `{block}x{tx}x{output}` | `ln:channel/{decimal id}` |

- A language prefix (`/de/tx/…`) and the API's own URLs (`/api/tx/{txid}`, `/api/v1/block/{hash}`,
  `/api/v1/lightning/nodes/{key}`, …) are the same object.
- Hashes, transaction ids and keys are written in lower case; bech32 addresses in lower case;
  legacy addresses as they are. Addresses must pass their checksum.
- Test networks are separate objects: `/testnet/…`, `/testnet4/…` and `/signet/…` links have the
  roots `btc-testnet:…`, `btc-testnet4:…`, `btc-signet:…` (and `ln-testnet:…`, `ln-signet:…`).
  A `bitcoin:` URI names a mainnet address only (it can't say which test network it means).
- A block link by height (`https://mempool.space/block/481824`) is read through the block's
  hash. It becomes an alias of `btc:block/{hash}` only once the block is in the best chain and
  six blocks deep; until then it keeps its own card.

An unknown block, transaction or channel is `missing`. mempool.space answers an unknown
Lightning node with a server error, as it does on a fault, so a server error on a node's record
is followed by the Lightning search for its exact key: a node the search doesn't list is
`missing`, and a listed node whose record keeps failing counts toward the failure limit.

## What it reads

- `GET https://mempool.space/api/v1/block/{hash}`, `/api/tx/{txid}`, `/api/address/{address}`,
  `/api/v1/lightning/nodes/{key}`, `/api/v1/lightning/channels/{id}` (with the network prefix,
  e.g. `/testnet4/api/…`);
- for a block by height, `/api/block-height/{height}` first, then `/api/block/{hash}/status` and
  `/api/blocks/tip/height`;
- when a node's record fails, `/api/v1/lightning/search?searchText={key}`;
- the fallback: the same paths (`/api/block/{hash}` for blocks) at
  `https://blockstream.info/api` (`/testnet/api`, `/signet/api`; no testnet4 and no Lightning).

## Card

The card types are local, not schema.org: `BitcoinBlock`, `BitcoinTransaction`,
`BitcoinAddress`, `LightningNode` and `LightningChannel`. `site` is "mempool.space".

- Block: `name` "Block 481,824", `start` the block time.
- Transaction: `name` "Transaction {txid}", `start` the block time once confirmed.
- Address: `name` the address.
- Lightning node: `name` its alias; when it was first seen is the `first_seen` fact. Its location
  is IP geolocation, not a place, so the card has no point.
- Lightning channel: `name` "Channel {short id}"; when it was opened and closed are the `created`
  and `closed` facts (an open channel has no `closed`).

| Fact | Meaning |
| --- | --- |
| `page_url` | The object's page on mempool.space |
| `height`, `hash`, `previous_block` | Block height, hash and previous block's hash |
| `tx_count`, `size`, `weight` | Transactions in the block, size in bytes, weight units |
| `pool` | The mining pool, as mempool.space names it |
| `total_fees`, `reward` | In satoshis |
| `median_fee_rate` | sat/vB |
| `confirmed`, `block_height`, `block_hash` | A transaction's confirmation |
| `fee`, `vsize`, `inputs`, `outputs`, `output_value`, `coinbase` | A transaction (satoshis, vbytes) |
| `received`, `balance`, `unconfirmed_tx_count` | An address (confirmed satoshis) |
| `public_key`, `alias`, `color`, `capacity`, `channels`, `updated_at` | A Lightning node (capacity in satoshis) |
| `first_seen` | When the Lightning node was first seen (RFC 3339) |
| `short_id`, `status`, `funding_tx`, `funding_output`, `closing_tx` | A Lightning channel (`active`, `inactive` or `closed`) |
| `created`, `closed` | When the channel was opened and closed (RFC 3339) |
| `node1`, `node1_alias`, `node2`, `node2_alias` | The channel's two nodes |

## Limits

mempool.space publishes no limit but answers 429 and then bans; requests go at one per second to
each of mempool.space and blockstream.info, and a 429 waits for its `Retry-After`.

## Examples

- `https://mempool.space/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d`
  (the pizza transaction)
- `https://mempool.space/block/481824` (SegWit activation, by height)
- `https://mempool.space/lightning/node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f`
  (ACINQ)
