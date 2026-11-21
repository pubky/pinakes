# mempool.space

Bitcoin blocks, transactions and addresses, and Lightning nodes and channels, read from the
mempool.space API by the identifier in the link. When mempool.space can't answer, blocks,
transactions and addresses are read from the Esplora API at blockstream.info instead (same
card). No key. Public blockchain and Lightning gossip data.

## Links

Every link of one object, on mempool.space or on another block explorer (below), is one
resource, with a root in the object's own scheme:

| Object | Links | Root |
| --- | --- | --- |
| Block | `https://mempool.space/block/{hash}` | `btc:block/{hash}` |
| Transaction | `https://mempool.space/tx/{txid}` (`#vin=…`/`#vout=…` included) | `btc:tx/{txid}` |
| Address | `https://mempool.space/address/{address}`, `bitcoin:{address}` (a payment request's query, `?amount=…&label=…&lightning=…&pj=…`, is dropped before the link is stored) | `btc:address/{address}` |
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

### Other block explorers

The same objects' pages on these explorers are the same resources, and the card is still read
from mempool.space: a tag on any of them counts on the one card. One table in `mempool.rs`
(`EXPLORERS`) holds them; it reads every form below and writes the card's Also on from the
first form of each listed site, so every listed link reads back as the card.

| Explorer | Networks (path prefix) | Block | Block by height | Transaction | Address | Also on |
| --- | --- | --- | --- | --- | --- | --- |
| Blockstream (`blockstream.info`) | mainnet, `/testnet`, `/testnet4`, `/signet` | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | listed |
| `mempool.emzy.de` (a mempool instance, language prefix as on mempool.space) | mainnet, `/testnet4`, `/signet` | `/block/{hash or n}` | | `/tx/{txid}` | `/address/{a}` | listed |
| BTCScan (`btcscan.org`) | mainnet | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | listed |
| Blockchain.com (`www.blockchain.com`) | `btc` | `/explorer/blocks/btc/{hash or n}` | | `/explorer/transactions/btc/{txid}` | `/explorer/addresses/btc/{a}` | listed |
| Blockchain.com, older paths | `/btc` | `/btc/block/{hash or n}` | | `/btc/tx/{txid}` | `/btc/address/{a}` | read only |
| Blockchain.com, `blockchain.info` | mainnet | `/block/{hash or n}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | read only |
| OKLink (`www.oklink.com`) | `/bitcoin`, `/btc` | `/{net}/block/{hash or n}` | | `/{net}/tx/{txid}` | `/{net}/address/{a}` | listed |
| Blockchair (`blockchair.com`) | `/bitcoin`, `/bitcoin/testnet` | `/{net}/block/{hash or n}` | | `/{net}/transaction/{txid}` | `/bitcoin/address/{a}` | read only |
| BlockCypher (`live.blockcypher.com`) | `/btc`, `/btc-testnet` | `/{net}/block/{hash}/` | | `/{net}/tx/{txid}/` | `/btc/address/{a}/` | read only |
| Bitcoin Explorer (`bitcoinexplorer.org`) | mainnet | `/block/{hash}` | `/block-height/{n}` | `/tx/{txid}` | `/address/{a}` | read only |
| 3xpl (`3xpl.com`) | `/bitcoin` | `/bitcoin/block/{hash or n}` | | `/bitcoin/transaction/{txid}` | `/bitcoin/address/{a}` | read only |

| Lightning explorer | Node | Channel | Also on |
| --- | --- | --- | --- |
| 1ML (`1ml.com`) | `/node/{key}` | `/channel/{id}` | listed |
| Amboss (`amboss.space`) | `/node/{key}` | `/edge/{id}` | listed |
| Lightning Network+ (`lightningnetwork.plus`) | `/nodes/{key}` | | listed |

- Blockstream (every network listed), BTCScan, Blockchain.com (its older paths and
  `blockchain.info` redirect to its pages) and mempool.emzy.de answer a wrong block, transaction
  or address with a 404 or a page that names nothing; 1ML and Lightning Network+ answer a wrong
  key with a 404. OKLink's pages render client-side (a wrong id also answers 200) and Amboss's
  `/edge/` is a route of its app, so their forms are routes only.
- Blockchair, BlockCypher, bitcoinexplorer.org and 3xpl refuse a plain client: their links are
  read, never listed.
- mempool.emzy.de's testnet (it answers 502) and Blockchain.com's `btc-testnet` pages (they show
  nothing that names their network) are left out.
- A trailing slash, a query string, a fragment and `www.` change nothing. Language prefixes are
  read only on mempool sites.
- Other chains on the same sites are not Bitcoin and name nothing: Blockstream's `/liquid/…`,
  Blockchain.com's `/bch/…` and `/eth/…`, Blockchair's `/bitcoin-cash/…` and `/ethereum/…`,
  BlockCypher's `/ltc/…` and `/doge/…`. Neither does a network an explorer doesn't show (BTCScan's
  testnet, mempool.emzy.de's testnet, Blockchain.com's `btc-testnet`, any Lightning object on
  testnet4).
- A block by height on any explorer is read like mempool.space's: an alias only once settled.

An unknown block, transaction or channel is `missing` when mempool.space itself says it is not
found. When mempool.space fails and the fallback does not know the object (a transaction its
node has not seen), the read ends with mempool.space's error: a rate limit or a server error is
retried later. mempool.space answers an unknown Lightning node with a server error, as it does
on a fault, so a server error on a node's record is followed by the Lightning search for its
exact key: a node the search doesn't list is `missing`, and a listed node whose record keeps
failing counts toward the failure limit. A 429 or 408 on a node's record is only retried later,
with no search. This is the one place a source falls back to a search endpoint, after its
object's own record failed, to decide `missing`; the match is exact on the key, never on a name.

## Also on

`mempool`: a block, transaction or address card lists its pages on Blockstream, mempool.emzy.de,
BTCScan, Blockchain.com and OKLink (those that show its network), and a Lightning node or
channel card its pages on 1ML, Amboss and Lightning Network+ (mainnet), from its root alone, with
no request. Each is a link of the card's own object (above).

## What it reads

- `GET https://mempool.space/api/v1/block/{hash}`, `/api/tx/{txid}`, `/api/address/{address}`,
  `/api/v1/lightning/nodes/{key}`, `/api/v1/lightning/channels/{id}` (with the network prefix,
  e.g. `/testnet4/api/…`);
- for a block by height, `/api/block-height/{height}` first, then `/api/block/{hash}/status` and
  `/api/blocks/tip/height`;
- when a node's record answers a server error, `/api/v1/lightning/search?searchText={key}`;
- for the card's `related` list (below), up to three more requests: for a block
  `/api/block/{hash}/status` (its next block), `/api/blocks/tip/height` (only when it has a next
  block, or was read by height) and `/api/block/{hash}/txid/0` (its coinbase transaction); for an
  address `/api/address/{address}/txs`; for a node
  `/api/v1/lightning/channels?public_key={key}&status=open`. A transaction's and a channel's
  come with their own record;
- the fallback: the same paths (`/api/block/{hash}` for blocks) at
  `https://blockstream.info/api` (`/testnet/api`, `/testnet4/api`, `/signet/api`). Lightning
  objects have no fallback: they are read from mempool.space only.

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
| `tx_count` | A block's transactions; an address's confirmed transactions |
| `size`, `weight` | A block's or a transaction's size in bytes and its weight units |
| `pool` | The mining pool, as mempool.space names it |
| `total_fees`, `reward` | In satoshis |
| `median_fee_rate` | sat/vB |
| `stale` | `true` for a block its status puts outside the best chain (an orphaned block) |
| `confirmed`, `block_height`, `block_hash` | A transaction's confirmation |
| `fee`, `vsize`, `inputs`, `outputs`, `output_value`, `coinbase` | A transaction (satoshis, vbytes) |
| `received`, `balance`, `unconfirmed_tx_count` | An address (confirmed satoshis) |
| `public_key`, `alias`, `color`, `capacity`, `channels`, `updated_at` | A Lightning node (capacity in satoshis) |
| `first_seen` | When the Lightning node was first seen (RFC 3339) |
| `short_id`, `status`, `funding_tx`, `funding_output`, `closing_tx` | A Lightning channel (`active`, `inactive` or `closed`) |
| `created`, `closed` | When the channel was opened and closed (RFC 3339) |
| `node1`, `node1_alias`, `node2`, `node2_alias` | The channel's two nodes |

## Related

Each card lists the objects it points at in the resource read's `related`
([Related objects](../api.md#related-objects)), each by its root, so a client opens their cards:

| Card | `rel` | From |
| --- | --- | --- |
| Transaction | `block` (`name` "Block 975,251", `at` the block time), once confirmed | its record |
| | `input`: the address each input spends from (`amount_sat` its value); a coinbase input has no `uri` and the `name` `coinbase` | its record (`vin[].prevout`) |
| | `output`: the address each output pays (`amount_sat`); an output without an address has no `uri` and a `name`: `OP_RETURN`, `P2PK` or the script type mempool.space names | its record (`vout[]`) |
| Block | `previous` (not on the genesis block) | its record |
| | `next`: the best chain's next block, once that block is six deep (as a block by height becomes an alias); a block in the best chain without one is read again within the hour | `/block/{hash}/status`, the tip |
| | `coinbase`: its first transaction (`amount_sat` the block reward, when mempool.space gives it) | `/block/{hash}/txid/0` |
| Address | `tx`: its latest transactions as the API lists them (those in the mempool first, then the newest confirmed), with `amount_sat` what each paid to the address minus what it spent from it, and `at` the block time once confirmed | `/address/{address}/txs` |
| Lightning node | `channel`: its biggest open channels (`amount_sat` the capacity, `name` the peer's alias), from mempool.space's first page of them; `peer`: those channels' other nodes, each once | `/v1/lightning/channels` |
| Lightning channel | `node`: its two nodes (`name` the alias); `funding_tx` (`amount_sat` the capacity); `closing_tx` once closed | its record |

- At most 10 inputs, outputs, transactions, channels and peers each, the largest amount first
  for inputs, outputs and channels (the API's order among equals); `related_total` gives how
  many inputs, outputs, transactions (the address's `tx_count` plus `unconfirmed_tx_count`) or
  channels there are when fewer are listed. A node's channel total is its `channels` fact, the
  channels mempool.space counts as active; the listed ones are its biggest open channels, which
  can include an inactive one. A transaction with 400 inputs is still one request: its record
  lists them all. A record over 4 MB (a few thousand inputs) is not read: the link fails and
  is retried like any failed read.
- Roots stay on the card's network: `btc-testnet4:address/…`, `ln-signet:node/…`,
  `btc-signet:tx/…` (a signet channel's funding transaction). An address that is not valid on
  the card's network is listed as its script type, without a `uri`.
- The extra requests are part of the read and paced like it, and share a budget of 8 seconds
  from the start of the read (the route allows 14): a part whose request is still waiting then
  is left out. Each part is left out when its request fails; a rate limit, a server error or
  the budget running out reads the card again within the hour, as does a transaction still in
  the mempool (its block comes later) and a block in the best chain whose next block is not
  settled yet (or that has none yet). A part refused for good (a 404, an answer that cannot be
  read) is left out until the card's own refresh. A read again that cannot fetch a part leaves
  it out until the read after. The Esplora fallback reads the same parts, but has no Lightning,
  and its block record has no `pool`, `total_fees`, `reward` or `median_fee_rate`: a block card
  read from it is read again within the hour, for mempool.space's record.
- An address's list is read up to 4 MB (a few dozen whole transactions); a larger one (an
  exchange's batch payouts) is left out. A part put off again and again (a server error, the
  budget) reads the card again every hour while it lasts.
- What a spent output was spent by (`/api/tx/{txid}/outspends`) is not read: one more request
  for every transaction card, and it changes after the card is read.

## Limits

mempool.space publishes no limit but answers 429 and then bans; requests go at one per second to
each of mempool.space and blockstream.info, and a 429 waits for its `Retry-After`.

## Examples

- `https://mempool.space/tx/a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d`
  (the pizza transaction)
- `https://mempool.space/block/481824` (SegWit activation, by height)
- `https://mempool.space/lightning/node/03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f`
  (ACINQ)
