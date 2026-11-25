# Open Food Facts

Pinakes reads products on the Open Food Facts databases from their API, by barcode. The databases are
Open Food Facts, Open Beauty Facts, Open Products Facts and Open Pet Food Facts. Their product pages
carry only the site's own structured data. No key is needed.

An also-on source of the same name finds the database's product for any other product card that
states a barcode ([below](#also-on)).

| | |
| --- | --- |
| Source | `open_food_facts`, official source with an also-on half |
| Links | `{country or world}.{database}/product/{barcode}[/{slug}]`, and `/produit/` |
| Fetches | `GET https://world.{database}/api/v2/product/{GTIN-13}.json?fields=…`, once or twice |
| Credentials | None |
| Rate | At most 1.66 requests per second on each database's `world.` host (the databases publish 100 product reads a minute). A source's rate only lowers the default, so with the default `limits.host_requests_per_second` (1) each host gets 1 |
| Refresh | Daily, as every official card ([refresh](../design.md#refresh)) |
| Card types | `Product` |
| Keys | `gtin:{GTIN-13}`, recorded by the `product_keys` also-on source |
| Credit | `© Open Food Facts contributors, ODbL 1.0` (or the other database's name), in `attribution` |
| Switch | `[sources.open_food_facts] enabled`, on by default; the also-on half `[also_on.sources.open_food_facts] enabled`, on by default |

## Links

The databases are on these hosts:

| Database | Host | Product type |
| --- | --- | --- |
| Open Food Facts | `openfoodfacts.org` | `food` |
| Open Beauty Facts | `openbeautyfacts.org` | `beauty` |
| Open Products Facts | `openproductsfacts.org` | `product` |
| Open Pet Food Facts | `openpetfoodfacts.org` | `petfood` |

Every address of one product is one resource, under the root
`https://world.{database}/product/{barcode}`. These links are [aliases](../api.md#aliases) of it:

- the page on a country's subdomain (`ch.openfoodfacts.org`) or on the bare host;
- the page with a slug after the barcode;
- the French `/produit/` in place of `/product/`.

Once the product is read, its root is its page on the database that holds it, with the barcode as
that database's record spells it. These links are then aliases of that root too:

- a link that spells the barcode with more or fewer leading zeros (`…/product/0000080466468` for
  `…/product/80466468`);
- a product page on a database that holds the barcode as another type of product
  (`https://world.openfoodfacts.org/product/80466468` for the product on openbeautyfacts.org).

A barcode is 8, 12, 13 or 14 digits with a valid check digit. A 14-digit one starts with `0`. Not
read here: an address with any other barcode, a category or list, and a host with a deeper subdomain.

## What it fetches

The first request goes to the database in the link:
`GET https://world.{database}/api/v2/product/{GTIN-13}.json?fields=code,product_name,brands,quantity,product_type,nutriscore_grade,image_front_small_url`.

A database that holds the barcode as another type of product answers 404 and names that type
("product found with a different product type: beauty"). The database for that type is then asked
once, and no other.

A barcode that the asked databases do not hold is `missing`, and so is a record of another barcode.
Any other answer is an error and the read is retried.

## Card

The type is `Product`.

- `site` is the database's name.
- `name` is the product name, else its brands.
- `image` is the address of the small front image.
- `attribution` is the database's credit. A client shows it with the card.

| Fact | Meaning |
| --- | --- |
| `gtin` | The barcode as GTIN-13 (the database may spell it shorter: `80466468`) |
| `brand` | The brands, as the database lists them |
| `quantity` | The quantity as the database writes it, for example `400 g` |
| `nutriscore` | `a` to `e`, for food |

## Keys and groups

The card's barcode is its key `gtin:{GTIN-13}`, recorded by the `product_keys` also-on source with
`open_food_facts` in `via`. `key=gtin:{barcode}` finds the card by the barcode in any of its lengths.
A [group](../api.md#groups) holds at most one `gtin:` value.

## Also on

The `open_food_facts` also-on source runs on a product card on any other site (a shop's page) that
states a barcode in its `gtin` fact. It asks the databases as above, with Open Food Facts first. The
database's record must be of that barcode.

What it records:

- a derived link to the product's page on the database, keyed by the barcode (`gtin:`);
- the facts `product_name`, `brands`, `quantity` and `nutriscore`, where the record has them;
- the fact `attribution`, the database's credit. Show it with these facts.

The link carries the key, and the card it was found for does not. A referenced resource that is that
database page takes the key.

An answer is kept for a day, and a failed request for at most five minutes. A 429 or a server error is
retried after its `Retry-After`, from one minute to an hour. The source's rows live 30 days and
expire after twice that.

## Credit

Each card, and the facts the also-on half records, carry
`© {database} contributors, ODbL 1.0`. That line names the licence the databases state for their
data, the Open Database License (ODbL 1.0).

## Turn it off

```toml
[sources.open_food_facts]
enabled = false

[also_on.sources.open_food_facts]
enabled = false
```

The two switches are independent. With the first off, links to the databases get ordinary web cards.
With the second off, the also-on half asks nothing, and after the restart its stored links and facts
stop showing.

## Examples

- `https://world.openfoodfacts.org/product/3017620422003/nutella-ferrero`
- `https://world.openbeautyfacts.org/product/80466468`
