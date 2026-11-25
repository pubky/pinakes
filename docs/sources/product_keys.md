# `product_keys`: a product's barcode as its key

This also-on source runs on [product](../api.md#products) cards that an
[official source](README.md) read from a service's own record. It makes the barcode in that record
the card's join key.

| | |
| --- | --- |
| Source | `product_keys`, also-on source |
| Runs on | `Product` cards (and subtypes) from an official source that is not a page hook, with a valid barcode in the `gtin` fact |
| Asks | No service |
| Credentials | None |
| Rate | None needed: it sends no request |
| Rows live | 30 days; rows expire after twice that |
| Keys | `gtin:{GTIN-13}` |
| Links | None |
| Facts | None |
| Switch | `[also_on.sources.product_keys] enabled`, on by default |

## What it does

1. The card must be a product, and an official source that is not a page hook must have read it.
2. The card's `gtin` fact must be a barcode of 8, 12, 13 or 14 digits with a valid check digit. A
   14-digit one starts with `0`.
3. The barcode, as GTIN-13, becomes the card's key `gtin:{GTIN-13}`. `via` names the official
   source.

Examples:

- A product database's record names barcode `3017620422003`: the card has the key
  `gtin:3017620422003`, via the source that read it.
- A shop's product page names `gtin13` `0840440422266` in its JSON-LD alone: the card has the fact
  `gtin` and no key.
- A Shopify shop's own product record, on the shop's own host, names barcode `0840440422266`: the
  card has the fact `gtin` and no key.

## Keys, links and facts

The key is exact: the service's own record states it. Cards that share a `gtin:` value join one
[group](../api.md#groups), which is a hint and merges nothing. A group holds at most one `gtin:`
value.

A barcode that only the page itself states is a fact and joins nothing. That holds for the page's
JSON-LD and for a record the page's own host serves, which a page hook reads.

`key=gtin:{barcode}` finds every link with that key, by the barcode in any of its lengths (UPC-A,
EAN-8, EAN-13, GTIN-14). A music release's barcode is the key `upc:`, which is separate from
`gtin:`.

## Cost

None. The source reads only the stored card.

## Turn it off

```toml
[also_on.sources.product_keys]
enabled = false
```

After the restart its stored keys stop showing: cards, search and groups leave them out. They are
kept until they expire, and show again when the source is on. The `gtin` fact stays on the card.
