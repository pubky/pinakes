# Pinakes

A Pubky index of external resources: the web pages, places, coordinates,
event pages, feeds and media that people reference on Pubky.

Nexus records references and never fetches. Pinakes reads Nexus's reference
feed, resolves each URI into one small typed document through a resolver per
kind, keeps the facts a query needs beside the public references, and answers
one search: text, type, map area, time, tags and reach. The rule for clients is
**Pinakes ranks, Nexus hydrates**.

The design is in [`docs/design.md`](docs/design.md). Maps and Events are the
first clients; each kind of client adds a resolver and its own logic on the
same base.

## Status

Rust workspace with the `pinakesd` binary, TOML/env configuration, sqlx migrations
and `/v1/info`. Ingestion and resource resolution follow in the next commit.

## Running locally

Install PostgreSQL 16, PostGIS and `pg_trgm`, create a `pinakes` database, then:

```sh
cp pinakes.example.toml pinakes-dev.toml
cargo run -p pinakesd -- --config pinakes-dev.toml
```

The daemon migrates on startup and listens on port 8090 by default. Config values
can be overridden with `PINAKES__KEY` or `PINAKES__SECTION__KEY`. The local config
is ignored by Git; keep credentials outside the repository.
