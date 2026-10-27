# Shopify

Shopify stores' product pages, improved from the store's own product record. A page hook: it
claims no links, and a store's pages are read by the web route as usual. No key.

## Links

A product page of a Shopify store: a page whose own script names its store
(`Shopify.shop = "….myshopify.com"`) at `[/{locale}][/collections/{collection}]/products/{handle}`,
with `?variant={id}` for one variant.

## What it reads

`GET {page without query}.json`: the store's product record, whose variants each name their
barcode, article number and price. Its JSON-LD often names none, or names the product group
only.

## Card

The page's own card (`Product` or `ProductGroup`, from its JSON-LD), as a
[product](../api.md#products) of the variant the link picks (`?variant=`), or of the product's
only variant. Without one variant picked, the hook adds nothing. The record's values replace the
page's for that variant: its barcode, article and part number entirely (a variant without a
valid barcode has none, never the page's other variant's), and its price with the offer's other
facts.

| Fact | Meaning |
| --- | --- |
| `gtin` | The variant's barcode as GTIN-13, when its check digit is right |
| `brand` | The product's vendor |
| `sku` | The variant's article number |
| `price`, `price_currency`, `price_at` | The variant's price, its currency and when it was read |

## Examples

- `https://www.allbirds.com/products/mens-tree-runners?variant=33179624669264`
