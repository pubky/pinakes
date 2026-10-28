# Open Food Facts

Products on the Open Food Facts databases (Open Food Facts, Open Beauty Facts, Open Products
Facts, Open Pet Food Facts), read from their API by barcode: their product pages carry only the
site's own structured data. No key; the databases publish a limit of 100 product reads a minute,
so each database's host is paced at 1.66 requests a second.

## Links

`https://world.{database}/product/{barcode}` is the root; a country's subdomain
(`ch.openfoodfacts.org`), a slug after the barcode and the French `/produit/` are aliases of it.
The databases are openfoodfacts.org, openbeautyfacts.org, openproductsfacts.org and
openpetfoodfacts.org.

## What it reads

`GET https://world.{database}/api/v2/product/{GTIN-13}.json?fields=…`. A database that holds the
barcode as another type of product names that type ("product found with a different product type:
beauty"), and that database is asked once. A barcode no database holds is `missing`.

## Card

A `Product` named by its product name, with its front image and the credit
`© Open Food Facts contributors, ODbL 1.0` (or the other database's) as `attribution`, which a
client shows with the card. Its barcode is its product key `gtin:`, as any product card's is
([also on](README.md#also-on)).

| Fact | Meaning |
| --- | --- |
| `gtin` | Its barcode as GTIN-13 (the database may spell it shorter: `80466468`) |
| `brand` | Its brands, as the database lists them |
| `quantity` | E.g. `400 g` |
| `nutriscore` | `a` to `e`, for food |

## Also on

`open_food_facts`: a product card on any other site that states its barcode (a shop's page) is
looked up the same way, Open Food Facts first. The database's record must be of that barcode. It
gives a derived link to the product there, keyed by the barcode (`gtin:`), and the facts
`product_name`, `brands`, `quantity`, `nutriscore` and `attribution` (the database's credit:
show it with them). Answers are cached for a day.

## Licence

The databases' data is under the Open Database License (ODbL 1.0), © their contributors; the
images under CC BY-SA. Pinakes keeps the image's address only.

## Examples

- `https://world.openfoodfacts.org/product/3017620422003/nutella-ferrero`
- `https://world.openbeautyfacts.org/product/80466468`
