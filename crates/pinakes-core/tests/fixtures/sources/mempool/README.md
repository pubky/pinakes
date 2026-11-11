mempool.space API responses, recorded once on 2026-10-19 with the Pinakes user agent
(`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every two seconds.
Parser regression fixtures for `pinakes_core::sources::mempool`; test runs make no requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Bodies over 64 KB are stored gzip-compressed (`.gz`).
The data is public blockchain and Lightning gossip data.

More answers were recorded on 2026-11-11 the same way for the cards' `related` lists (a block's
status and coinbase id, block 1 and block 975,251, the tip, a coinbase transaction with an
OP_RETURN output, a testnet4 coinbase transaction, an address's transaction list and a node's
channel list); `provenance.json` lists them with their times.
