# `product_keys`: a product's barcode as its key

An [also on](../api.md#also-on) source.

`product_keys` reads a [product](../api.md#products) card whose barcode (`gtin`) a service's own
record states (a product database's record, read by an [official source](README.md)) and makes
it the key `gtin:{GTIN-13}`: two shops' pages of one product join one group, a hint like every
group. `via` names that source. A barcode only the page's own JSON-LD states, or a record the
page's own host serves (a shop's product record, read by a page hook), is the card's fact `gtin`
and makes no key: what a page says about itself never joins it to another resource. A page that names several products of its own (in its
head, or naming the page; one product described twice is one), a product it only lists (named by
a fragment of a page whose head declares it a collection or about another entity), or a barcode
with a wrong check digit, states none. It asks no service.

`key=gtin:{barcode}` searches every link with the barcode, in any of its lengths (UPC-A, EAN-8,
EAN-13, GTIN-14).

A product's price and availability are facts of its own card with the time they were read
(`price_at`), never keys. A music release's barcode is the music sources' `upc:` key and stays
apart from `gtin:`.

## Examples

- A product database's record naming barcode `3017620422003`: key `gtin:3017620422003`, via
  the source that read it.
- A shop's product page whose JSON-LD alone names `gtin13` `0840440422266`: `facts.gtin`, no key.
- A Shopify shop's product whose own product record (on the shop's own host) names barcode
  `0840440422266`: `facts.gtin`, no key.
