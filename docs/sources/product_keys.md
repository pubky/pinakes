# `product_keys`: a product's barcode as its key

An [also on](../api.md#also-on) source.

`product_keys` reads a [product](../api.md#products) card that states its own barcode (`gtin`, from
its page's JSON-LD, its shop's own product record or a product database's record) and makes it
the key `gtin:{GTIN-13}`: two shops' pages of one product join one group, a hint like every
group. `via` names the record that stated it: an official source by name, else `page`. A page
that names several products of its own (in its head, or naming the page), or a barcode with a
wrong check digit, states none. It asks no service.

`key=gtin:{barcode}` searches every link with the barcode, in any of its lengths (UPC-A, EAN-8,
EAN-13, GTIN-14).

A product's price and availability are facts of its own card with the time they were read
(`price_at`), never keys. A music release's barcode is the music sources' `upc:` key and stays
apart from `gtin:`.

## Examples

- A shop's product page whose JSON-LD names `gtin13` `0840440422266`: key `gtin:0840440422266`.
