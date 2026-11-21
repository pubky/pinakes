Esplora API responses from blockstream.info, recorded once on 2026-10-19 with the Pinakes user
agent (`pinakes/0.1.0 (+https://pubky.app; info@synonym.to)`), about one request every two
seconds. Parser regression fixtures for `pinakes_core::sources::esplora`; test runs make no
requests.

`provenance.json` lists each fixture's request URL, status, content type, recording time, and the
size and SHA-256 of the body as served. Bodies over 64 KB are stored gzip-compressed (`.gz`).
The data is public blockchain data.

More answers were recorded on 2026-11-11 the same way for the cards' `related` lists (a block's
status and coinbase id, an address's transaction list); `provenance.json` lists them.

A testnet4 coinbase transaction (`testnet4-tx-coinbase-1000.json`, block 1,000's first) was
recorded on 2026-11-21 the same way, with `pinakes/0.1.0 (+https://pubky.app)`, for the testnet4
fallback.
