# Shopify

Pinakes improves Shopify stores' product pages from the store's own product record, which names each
variant's barcode where the page's JSON-LD often does not. This source is a page hook: it claims no
links, and the web route reads a store's pages as usual. No key is needed.

| | |
| --- | --- |
| Source | `shopify`, official source, page hook |
| Links | None claimed: product pages of Shopify stores, found by what the fetched page says |
| Fetches | `GET {page address without its query}.json`, from the page's own host |
| Credentials | None |
| Rate | Shared per-host default (`limits.host_requests_per_second`) |
| Refresh | With the page ([refresh](../design.md#refresh)) |
| Card types | The page's own: `Product`, `ProductGroup` or another product type; `Product` for a page that is no product |
| Keys | None (facts only) |
| Credit | None |
| Switch | `[sources.shopify] enabled`, on by default |

## Links

The hook applies to a page that meets all of these conditions:

- It answered 200 with HTML, and its first 512 KiB name the store (`Shopify.shop = …`).
- Its address, after redirects, ends in `/products/{handle}`. At most three path segments come
  before `products`, for example a locale or `collections/{collection}`.
- The handle and those segments hold only letters, digits, `-`, `_` and `%`, and each is at most 255
  characters.

`?variant={id}` in the address picks one variant. The hook adds no
[aliases](../api.md#aliases).

Not read here: a store's lists, its `/products` page and any page below a product.

## What it fetches

One request per page read: `GET {page address without its query}.json`, the store's product record.
Each variant in it can name a barcode, an article number and a price.

The hook runs within the web fetch's deadline and at most 3 seconds. The page keeps its own card
when the record answers anything but 200, cannot be read, or picks no variant. The record picks the
variant that the address names, or the product's only variant.

## Card

The card is the page's own card, as a [product](../api.md#products) of the picked variant. `from`
names `shopify`. `name` is the record's title only when the page gives none.

The record's values replace the page's:

- The barcode, article number and part number come from the record alone. A variant without a valid
  barcode has none, and never the page's barcode, which can be another variant's.
- When the record gives a price, the offer is the record's: the page's price, currency, availability
  and seller are dropped. When it gives none, the page's offer stays.
- The record's vendor replaces the page's brand.

| Fact | Meaning |
| --- | --- |
| `gtin` | The variant's barcode as GTIN-13, when its check digit is right |
| `brand` | The product's vendor, else the page's brand |
| `sku` | The variant's article number |
| `price` | The variant's price: a number that is not negative |
| `price_currency` | The price's currency: three letters, in upper case |
| `price_at` | When the price was read (RFC 3339) |
| `availability`, `seller` | The page's own, only when the record gives no price |
| `mpn`, `asin`, `isbn` | Facts of every product. The record gives none, and the page's `mpn` is dropped: absent |

## Keys and groups

The source emits no keys. The page's own host serves the record, so its barcode is what the page says
about itself: the fact `gtin`, never the key `gtin:`. It joins nothing.

## Credit

The card shows no credit line.

## Turn it off

```toml
[sources.shopify]
enabled = false
```

Shopify product pages then keep their own web cards.

## Examples

- `https://www.allbirds.com/products/mens-tree-runners?variant=33179624669264`
