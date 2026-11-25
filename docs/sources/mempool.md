# mempool.space

Pinakes reads Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, from the
mempool.space API, by the identifier in the link. When mempool.space cannot answer, blocks,
transactions and addresses are read from the Esplora API at blockstream.info instead. No key is
needed.

Links on other block explorers name the same objects. An also-on source of the same name lists the
object's pages on those explorers ([below](#also-on)).

| | |
| --- | --- |
| Source | `mempool`, official source with an also-on half |
| Links | Block, transaction, address, node and channel pages on mempool.space and [other explorers](#other-block-explorers); `bitcoin:{address}`; `btc:`, `btc-testnet:`, `btc-testnet4:`, `btc-signet:`, `ln:`, `ln-testnet:` and `ln-signet:` roots |
| Fetches | `GET https://mempool.space/api/…` for the object's record, then the requests for its [related objects](#related-objects); for on-chain objects, the same at `https://blockstream.info/api/…` as a fallback |
| Credentials | None |
| Rate | 1 request per second on `mempool.space` and 1 on `blockstream.info` |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)); within the hour while a [part is not there yet](#when-a-card-is-read-again). A block or a confirmed transaction is final a day after its block time |
| Card types | `BitcoinBlock`, `BitcoinTransaction`, `BitcoinAddress`, `LightningNode`, `LightningChannel` (local types, not schema.org) |
| Keys | None. Links join as aliases of the object's root |
| Credit | None |
| Switch | `[sources.mempool] enabled`, on by default; the also-on half `[also_on.sources.mempool] enabled`, on by default |

## Links

Every link of one object is one resource, under a root in the object's own scheme. The links below,
on mempool.space or on another explorer, are [aliases](../api.md#aliases) of the root:

| Object | Links on mempool.space | Root |
| --- | --- | --- |
| Block | `https://mempool.space/block/{hash}` | `btc:block/{hash}` |
| Transaction | `https://mempool.space/tx/{txid}` (with `#vin=…` or `#vout=…` too) | `btc:tx/{txid}` |
| Address | `https://mempool.space/address/{address}`, `bitcoin:{address}` | `btc:address/{address}` |
| Lightning node | `https://mempool.space/lightning/node/{public key}` | `ln:node/{public key}` |
| Lightning channel | `https://mempool.space/lightning/channel/{id}`, where `{id}` is the decimal ID or the short ID `{block}x{tx}x{output}` | `ln:channel/{decimal id}` |

On mempool.space:

- A two-letter language prefix (`/de/tx/…`) changes nothing.
- The API's own addresses name the same objects: `/api/v1/block/{hash}`, `/api/block/{hash}`,
  `/api/block-height/{height}`, `/api/tx/{txid}`, `/api/address/{address}`,
  `/api/v1/lightning/nodes/{key}` and `/api/v1/lightning/channels/{id}`.

On every site, a trailing slash, a query, a fragment and `www.` change nothing. A link with a port is
not read.

### Identifiers

- A block hash or a transaction ID is 64 hexadecimal digits. A node key is 66, starting with `02` or
  `03`. Roots write them in lower case.
- An address must pass its checksum and belong to the network. Bech32 addresses are written in lower
  case, legacy addresses as they are.
- A `bitcoin:` URI names a mainnet address only, because it cannot say which test network it means.
  A payment request's query (`?amount=…&label=…&lightning=…&pj=…`) is dropped before the link is
  stored.
- A root that is not in this form is `missing` at once and never fetched.

### Test networks

Each test network has its own objects. Links under `/testnet/…`, `/testnet4/…` and `/signet/…` have
the roots `btc-testnet:…`, `btc-testnet4:…` and `btc-signet:…`. Lightning objects there have
`ln-testnet:…` and `ln-signet:…`. No Lightning object is on testnet4.

### Blocks by height

A block link by height (`https://mempool.space/block/481824`, on any explorer) is read through the
block's hash. It becomes an alias of `btc:block/{hash}` only once the block is in the best chain and
six blocks deep, its own block counted. Until then it keeps its own card.

### Other block explorers

The same objects' pages on these explorers are the same resources. Their cards are still read from
mempool.space, so a reference on any of them counts on the one card. The table `EXPLORERS` in
`mempool.rs` holds every form below. `{n}` is a block height.

| Explorer | Networks (path prefix `{net}`) | Block | Block by height | Transaction | Address | Also on |
| --- | --- | --- | --- | --- | --- | --- |
| Blockstream (`blockstream.info`) | mainnet, `/testnet`, `/testnet4`, `/signet` | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | Listed |
| `mempool.emzy.de` (a mempool instance, language prefix as on mempool.space) | mainnet, `/testnet4`, `/signet` | `/block/{hash or n}` | | `/tx/{txid}` | `/address/{a}` | Read only |
| BTCScan (`btcscan.org`) | mainnet | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | Listed |
| Blockchain.com (`www.blockchain.com`) | mainnet (`btc`) | `/explorer/blocks/btc/{hash or n}` | | `/explorer/transactions/btc/{txid}` | `/explorer/addresses/btc/{a}` | Listed |
| Blockchain.com, older paths | mainnet (`btc`) | `/btc/block/{hash or n}` | | `/btc/tx/{txid}` | `/btc/address/{a}` | Read only |
| Blockchain.com (`blockchain.info`) | mainnet | `/block/{hash or n}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | Read only |
| OKLink (`www.oklink.com`) | mainnet (`bitcoin`, `btc`) | `/{net}/block/{hash or n}` | | `/{net}/tx/{txid}` | `/{net}/address/{a}` | Listed |
| Blockchair (`blockchair.com`) | mainnet (`bitcoin`), testnet (`bitcoin/testnet`) | `/{net}/block/{hash or n}` | | `/{net}/transaction/{txid}` | `/bitcoin/address/{a}` (mainnet only) | Read only |
| BlockCypher (`live.blockcypher.com`) | mainnet (`btc`), testnet (`btc-testnet`) | `/{net}/block/{hash}` | | `/{net}/tx/{txid}` | `/btc/address/{a}` (mainnet only) | Read only |
| Bitcoin Explorer (`bitcoinexplorer.org`) | mainnet | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | Read only |
| 3xpl (`3xpl.com`) | mainnet (`bitcoin`) | `/bitcoin/block/{hash or n}` | | `/bitcoin/transaction/{txid}` | `/bitcoin/address/{a}` | Read only |

| Lightning explorer (mainnet) | Node | Channel | Also on |
| --- | --- | --- | --- |
| 1ML (`1ml.com`) | `/node/{key}` | `/channel/{id}` | Listed |
| Amboss (`amboss.space`) | `/node/{key}` | `/edge/{id}` | Listed |
| Lightning Network+ (`lightningnetwork.plus`) | `/nodes/{key}` | | Listed |

"Read only" means that a link there reads as the card, but the card does not list the site. The sites
that refuse a plain client, and `mempool.emzy.de`, a copy of mempool.space, are read only.

Each form was checked on the live site where the site shows the object, as a route where its pages
render client-side, and on archived captures for the sites that refuse a plain client. Two networks
are left out: `mempool.emzy.de`'s testnet, which answers 502, and Blockchain.com's `btc-testnet`
pages, which show nothing that names their network.

Other chains on the same sites are not Bitcoin and name nothing here, for example
Blockstream's `/liquid/…`, Blockchain.com's `/bch/…` and `/eth/…`, Blockchair's `/bitcoin-cash/…` and
`/ethereum/…`, and BlockCypher's `/ltc/…` and `/doge/…`. Neither does a network that an explorer does
not show.

## What it fetches

### The object's record

| Object | Request at `https://mempool.space` (with the network's prefix, such as `/testnet4/api/…`) |
| --- | --- |
| Block | `GET /api/v1/block/{hash}` |
| Block by height | `GET /api/block-height/{height}` first, then the block's record, `GET /api/block/{hash}/status` and `GET /api/blocks/tip/height` |
| Transaction | `GET /api/tx/{txid}` |
| Address | `GET /api/address/{address}` |
| Lightning node | `GET /api/v1/lightning/nodes/{key}` |
| Lightning channel | `GET /api/v1/lightning/channels/{id}` |

Further requests for the card's related objects are in
[Related objects](#related-objects). A transaction's record and an address's transaction list are
read up to 4 MB. A larger transaction record is an error, and the read is retried.

### Fallback

When mempool.space fails the record of a block, transaction or address (for example a server error, a
rate limit or an answer that cannot be read), the source asks blockstream.info's Esplora API with the same paths:

| Network | Base |
| --- | --- |
| mainnet | `https://blockstream.info/api` |
| testnet | `https://blockstream.info/testnet/api` |
| testnet4 | `https://blockstream.info/testnet4/api` |
| signet | `https://blockstream.info/signet/api` |

A block's record is `/block/{hash}` there. The fallback reads the same card, except that its block
record has no `pool`, `total_fees`, `reward` or `median_fee_rate`. Lightning objects have no
fallback.

### Missing objects

- A block, transaction, channel or node is `missing` when mempool.space answers 404 for its record,
  and so is a block height the chain has not reached. Every valid address exists, so an address is
  never `missing`.
- When mempool.space fails and the fallback does not know the object, the read ends with
  mempool.space's error. It is retried, because a transaction that the fallback's node has not seen
  can still exist.
- mempool.space answers a server error for an unknown Lightning node as well as on a fault. After a
  server error on a node's record, the source asks `GET /api/v1/lightning/search?searchText={key}` for the exact key.
  A node the search does not list is `missing`. A listed node whose record keeps failing counts
  toward the failure limit. A 429 or 408 is only retried, with no search. The match is on the key,
  never on a name.

### Retries

A rate limit (429), a request timeout (408) or a server error makes the read wait the longer of the
answer's `Retry-After` and the queue's backoff, which grows while the answers last.

## Card

`site` is "mempool.space" on every card. `page_url` is the object's page on mempool.space, also when
the link is on another explorer.

Amounts are whole numbers of bitcoin as BIP 177 counts them: the base unit, ₿
(₿100,000,000 = 1 BTC).

| Type | `name` | `start` |
| --- | --- | --- |
| `BitcoinBlock` | "Block 481,824" | The block time |
| `BitcoinTransaction` | "Transaction {txid}" | The block time, once confirmed |
| `BitcoinAddress` | The address | |
| `LightningNode` | The node's alias, else its key | |
| `LightningChannel` | "Channel {short id}" | |

A Lightning node's location is IP geolocation, not a place, so its card has no point.

### Block facts

| Fact | Meaning |
| --- | --- |
| `page_url` | The block's page on mempool.space |
| `height`, `hash`, `previous_block` | The block's height, its hash and the previous block's hash |
| `tx_count` | The number of transactions |
| `size`, `weight` | The size in bytes and the weight units |
| `pool` | The mining pool, as mempool.space names it |
| `total_fees`, `reward` | In bitcoin |
| `median_fee_rate` | In bitcoin per virtual byte (₿/vB) |
| `stale` | `true` for a block that its status puts outside the best chain, else absent |

### Transaction facts

| Fact | Meaning |
| --- | --- |
| `page_url` | The transaction's page on mempool.space |
| `confirmed` | Whether it is in a block |
| `block_height`, `block_hash` | The block it is in, once confirmed |
| `fee`, `output_value` | The fee and the sum of its outputs, in bitcoin |
| `size`, `vsize`, `weight` | The size in bytes, in virtual bytes and in weight units |
| `inputs`, `outputs` | How many it has |
| `coinbase` | Whether it is a block's coinbase transaction |

### Address facts

| Fact | Meaning |
| --- | --- |
| `page_url` | The address's page on mempool.space |
| `received`, `balance` | Confirmed bitcoin received, and what is left of it |
| `tx_count`, `unconfirmed_tx_count` | Confirmed transactions, and those in the mempool |

### Lightning node facts

| Fact | Meaning |
| --- | --- |
| `page_url` | The node's page on mempool.space |
| `public_key`, `alias`, `color` | The node's key, alias and color |
| `capacity` | Its capacity, in bitcoin |
| `channels` | Its active channels |
| `first_seen`, `updated_at` | When the node was first seen and last updated (RFC 3339) |

### Lightning channel facts

| Fact | Meaning |
| --- | --- |
| `page_url` | The channel's page on mempool.space |
| `short_id` | `{block}x{tx}x{output}` |
| `status` | `active`, `inactive` or `closed` |
| `capacity` | Its capacity, in bitcoin |
| `created`, `closed` | When it was opened and closed (RFC 3339). An open channel has no `closed` |
| `funding_tx`, `funding_output`, `closing_tx` | The transaction and output that opened it, and the one that closed it |
| `node1`, `node1_alias`, `node2`, `node2_alias` | Its two nodes' keys and aliases |

### Related objects

A card's `related` lists the objects it points at ([Related objects](../api.md#related-objects)).
Each entry has a `rel` and the object's root in `uri`, so a client opens that card.

| Card | `rel` | Entry | Read from |
| --- | --- | --- | --- |
| Transaction | `block` | Its block, once confirmed (`name` "Block 975,251", `at` the block time) | The transaction's record |
| | `input` | The address each input spends from (`amount_sat` its value). A coinbase input has no `uri` and the `name` `coinbase` | The transaction's record |
| | `output` | The address each output pays (`amount_sat`). An output without an address has no `uri` and a `name`: `OP_RETURN`, `P2PK` or the script type mempool.space names | The transaction's record |
| Block | `previous` | The previous block (none on the genesis block) | The block's record |
| | `next` | The best chain's next block, once that block is six blocks deep | `/block/{hash}/status`, then `/blocks/tip/height` when it has a next block |
| | `coinbase` | Its first transaction (`amount_sat` the block reward, when the record gives it) | `/block/{hash}/txid/0` |
| Address | `tx` | Its latest transactions in the API's order (those in the mempool first). `amount_sat` is what each paid to the address minus what it spent from it, `at` the block time once confirmed | `/address/{address}/txs` |
| Lightning node | `channel` | Its biggest open channels (`amount_sat` the capacity, `name` the peer's alias) | `/v1/lightning/channels?public_key={key}&status=open` |
| | `peer` | Those channels' other nodes, each once (`name` the alias) | The same answer |
| Lightning channel | `node` | Its two nodes (`name` the alias) | The channel's record |
| | `funding_tx` | The transaction that opened it (`amount_sat` the capacity) | The channel's record |
| | `closing_tx` | The transaction that closed it, once closed | The channel's record |

- Each `rel` lists at most 10 entries. Inputs, outputs and channels come largest amount first, in the
  API's order among equals. When there are more, `related_total` gives the number: inputs, outputs,
  an address's transactions (`tx_count` plus `unconfirmed_tx_count`) or a node's channels.
- A node's total is its `channels` fact, which counts active channels. The list holds its biggest
  open channels, which can include an inactive one.
- Roots stay on the card's network (`btc-testnet4:address/…`, `ln-signet:node/…`). An address that is
  not valid there is listed by its script type, without a `uri`.
- What a spent output was spent by is not read.

### When a card is read again

The requests for related objects are part of the read and share a budget of 8 seconds from its start.
A part whose request fails or is still waiting then is left out. The card is read again:

- within the hour, when a request was put off (a rate limit, a server error or the budget);
- within the hour, while a transaction is in the mempool;
- within the hour, while a block in the best chain has no next block, or its next block is not six
  blocks deep;
- within the hour, when the block's record came from the fallback;
- otherwise at its usual refresh. A part refused for good (a 404, an answer that cannot be read, a
  transaction list over 4 MB) stays out until then.

A part that keeps being put off keeps the card reading hourly. A block read by height needs its
status and the tip: when either fails, the read fails.

## Keys and groups

The source emits no keys. The links of one object, on every explorer, join as aliases of its root.
Each entry in `related` is another object and joins nothing.

## Also on

The `mempool` also-on source runs on the card of a root (`btc:…` or `ln:…`, or a test network's) once
it has been read. From the root alone, with no request, it adds a derived link to the object's page
on each listed explorer that shows its network:

| Card | Sites, in this order |
| --- | --- |
| Block, transaction, address | Blockstream Explorer (every network), BTCScan, Blockchain.com, OKLink (mainnet) |
| Lightning node | 1ML, Amboss, Lightning Network+ (mainnet) |
| Lightning channel | 1ML, Amboss (mainnet) |

Each link is an alias of the card's own root. `via` is `mempool`. The links carry no keys. Its rows
live 30 days and expire after twice that.

## Credit

The card shows no credit line.

## Turn it off

```toml
[sources.mempool]
enabled = false

[also_on.sources.mempool]
enabled = false
```

The two switches are independent. With the first off, web links get ordinary web cards, and
`bitcoin:` links and the `btc:…` and `ln:…` roots wait until the source is on again. With the second
off, cards list no explorer pages: after the restart the stored links stop showing.

## Examples

- `https://mempool.space/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d`
  (the pizza transaction)
- `https://mempool.space/block/481824` (SegWit activation, by height)
- `https://mempool.space/lightning/node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f`
  (ACINQ)
