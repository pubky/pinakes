//! Products (`Product` and its subtypes): the barcode and what a shop says about one, from the
//! page's own JSON-LD or the shop's own product record (a page hook in `sources`).
//!
//! The barcode (`gtin`) is the product's GTIN-13 (`barcode::gtin13`), from the product's own
//! `gtin`, `gtin8`, `gtin12`, `gtin13`, `gtin14` or `isbn`, never from text. The also-on
//! sources make it the key `gtin:` that groups the same product on several shops. A price and an
//! availability are facts with the time they were read (`price_at`), never identity.
//!
//! Amazon pages are never fetched: an Amazon product link's card comes from the link alone
//! (`amazon`).
use crate::model::ResourceMeta;
use serde_json::Value;
use url::Url;

pub const TYPE: &str = "Product";

/// The attributes a product publishes in `facts`.
pub const FACTS: &[&str] = &[
    // GTIN-13.
    "gtin",
    "brand",
    // The shop's own article number and the maker's part number.
    "sku",
    "mpn",
    // The lowest price the page offers, its ISO 4217 currency, its availability (`in_stock`,
    // `out_of_stock`, `pre_order`, `back_order`, `discontinued`, `limited_availability`,
    // `sold_out`, `in_store_only`, `online_only`) and when they were read (RFC 3339).
    "price",
    "price_currency",
    "availability",
    "price_at",
    "seller",
    // An Amazon link's ASIN.
    "asin",
];

pub fn is_product(doc: &ResourceMeta) -> bool {
    doc.type_path.iter().any(|t| t == TYPE)
}

/// What a product page says about its product.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Product {
    pub gtin: Option<String>,
    pub brand: Option<String>,
    pub sku: Option<String>,
    pub mpn: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub availability: Option<String>,
    pub seller: Option<String>,
    /// Plain text.
    pub description: Option<String>,
}
impl Product {
    /// Record these facts on `doc` in place of what it holds for them: a more exact record (one
    /// variant's) than the page's.
    pub fn replace(self, doc: &mut ResourceMeta) {
        for (key, given) in [
            ("gtin", self.gtin.is_some()),
            ("brand", self.brand.is_some()),
            ("sku", self.sku.is_some()),
            ("mpn", self.mpn.is_some()),
            ("price", self.price.is_some()),
            ("price_currency", self.price.is_some()),
            ("price_at", self.price.is_some()),
            ("availability", self.availability.is_some()),
            ("seller", self.seller.is_some()),
        ] {
            if given {
                doc.attributes.remove(key);
            }
        }
        self.apply(doc);
    }
    /// Record these facts on `doc`, keeping what it already holds; prices are dated with the
    /// card's read.
    pub fn apply(self, doc: &mut ResourceMeta) {
        if doc.description.is_none() {
            doc.description = self.description;
        }
        let priced = self.price.is_some() || self.availability.is_some();
        let at = chrono::DateTime::from_timestamp_millis(doc.fetched_at)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        for (key, value) in [
            ("gtin", self.gtin),
            ("brand", self.brand),
            ("sku", self.sku),
            ("mpn", self.mpn),
            ("price", self.price.map(amount)),
            (
                "price_currency",
                self.currency.filter(|_| self.price.is_some()),
            ),
            ("availability", self.availability),
            ("price_at", at.filter(|_| priced)),
            ("seller", self.seller),
        ] {
            if let Some(value) = value {
                doc.attributes.entry(key.into()).or_insert(value);
            }
        }
    }
}

/// The product a JSON-LD `Product` node describes on the page at `page`: for a `ProductGroup`,
/// the variant the page's address names (its `url` or `@id`, or Shopify's `?variant=`), else its
/// only variant.
pub fn from_jsonld(v: &Value, page: &Url) -> Product {
    let variant = variant(v, page);
    let node = variant.unwrap_or(v);
    let offers: Vec<&Value> = all(&node["offers"])
        .chain(variant.map_or_else(Vec::new, |_| all(&v["offers"]).collect()))
        .flat_map(|o| std::iter::once(o).chain(all(&o["offers"])))
        .collect();
    let gtin = gtin(node).or_else(|| {
        // The offer of this one product may carry its barcode instead.
        let codes: Vec<String> = offers.iter().filter_map(|o| gtin(o)).collect();
        (!codes.is_empty() && codes.iter().all(|c| *c == codes[0])).then(|| codes[0].clone())
    });
    let prices: Vec<(f64, Option<String>)> = offers
        .iter()
        .filter_map(|o| {
            number(&o["price"])
                .or_else(|| number(&o["lowPrice"]))
                .or_else(|| number(&o["priceSpecification"]["price"]))
                .map(|p| {
                    let currency = text(&o["priceCurrency"])
                        .or_else(|| text(&o["priceSpecification"]["priceCurrency"]));
                    (p, currency)
                })
        })
        .collect();
    let lowest = prices
        .iter()
        .cloned()
        .reduce(|a, b| if b.0 < a.0 { b } else { a });
    let brand = first(&node["brand"]);
    let brand = text(brand).or_else(|| text(&brand["name"])).or_else(|| {
        let brand = first(&v["brand"]);
        text(brand).or_else(|| text(&brand["name"]))
    });
    let seller = offers
        .iter()
        .find_map(|o| text(&o["seller"]["name"]).or_else(|| text(&o["seller"])));
    Product {
        gtin,
        brand,
        sku: text(&node["sku"]).or_else(|| text(&v["sku"])),
        mpn: text(&node["mpn"]).or_else(|| text(&v["mpn"])),
        price: lowest.as_ref().map(|l| l.0),
        currency: lowest
            .and_then(|l| l.1)
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())),
        availability: offers
            .iter()
            .find_map(|o| text(&o["availability"]).and_then(|a| availability(&a))),
        seller,
        description: text(&node["description"])
            .or_else(|| text(&v["description"]))
            .filter(|d| d.contains('<'))
            .and_then(|d| crate::kinds::jobs::excerpt(&d)),
    }
}

/// The variant of a `ProductGroup` that the page names.
fn variant<'a>(v: &'a Value, page: &Url) -> Option<&'a Value> {
    let variants: Vec<&Value> = all(&v["hasVariant"]).filter(|n| n.is_object()).collect();
    if variants.is_empty() {
        return None;
    }
    let chosen = page
        .query_pairs()
        .find(|(k, _)| k == "variant")
        .map(|(_, id)| id.into_owned());
    let named = |n: &&&Value| {
        ["url", "@id"]
            .iter()
            .filter_map(|k| n[*k].as_str())
            .filter_map(|u| page.join(u).ok())
            .any(|u| {
                u == *page
                    || chosen
                        .as_deref()
                        .is_some_and(|id| u.query_pairs().any(|(k, v)| k == "variant" && v == id))
            })
    };
    variants
        .iter()
        .find(named)
        .copied()
        .or_else(|| (variants.len() == 1).then(|| variants[0]))
}

/// A node's own barcode, as GTIN-13: its `gtin…` property, else the barcode of its `isbn`.
pub fn gtin(v: &Value) -> Option<String> {
    ["gtin13", "gtin", "gtin12", "gtin14", "gtin8"]
        .iter()
        .filter_map(|k| text(&v[*k]))
        .find_map(|code| {
            // A GS1 Digital Link or a URN spelling ends with the number.
            let digits = code
                .rsplit(['/', ':'])
                .next()
                .unwrap_or_default()
                .replace([' ', '-'], "");
            crate::barcode::gtin13(&digits)
        })
        .or_else(|| text(&v["isbn"]).and_then(|i| crate::barcode::of_isbn(&i)))
}

/// A schema.org availability (`https://schema.org/InStock`, `InStock`) in snake case.
pub fn availability(s: &str) -> Option<String> {
    let name = s.trim().rsplit(['/', ':']).next().unwrap_or_default();
    let known = [
        ("InStock", "in_stock"),
        ("OutOfStock", "out_of_stock"),
        ("PreOrder", "pre_order"),
        ("PreSale", "pre_order"),
        ("BackOrder", "back_order"),
        ("Discontinued", "discontinued"),
        ("LimitedAvailability", "limited_availability"),
        ("SoldOut", "sold_out"),
        ("InStoreOnly", "in_store_only"),
        ("OnlineOnly", "online_only"),
        ("Reserved", "reserved"),
        ("MadeToOrder", "made_to_order"),
    ];
    known
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| (*v).to_owned())
}

/// The card of an Amazon link, from the link alone: Amazon pages are never fetched. A product
/// (`/dp/{ASIN}`, `/gp/product/{ASIN}`, with or without a slug before it) is a `Product`
/// carrying its ASIN (and, for a book, the ISBN-10 an ASIN of ten digits is); any other Amazon
/// page is a plain `WebPage`. `None` for a link that is not on Amazon's shop.
pub fn amazon(uri: &str, ttl: u64) -> Option<ResourceMeta> {
    let url = Url::parse(uri).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if !AMAZON.contains(&host) {
        return None;
    }
    let mut doc = ResourceMeta::new(uri, "web", ttl);
    doc.site = "Amazon".into();
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let asin = segments.windows(2).find_map(|w| match w {
        ["dp", id] | ["product", id] => Some(*id),
        _ => None,
    });
    if let Some(asin) = asin.filter(|a| {
        a.len() == 10
            && a.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    }) {
        doc.set_type(TYPE);
        doc.attributes.insert("asin".into(), asin.into());
        if let Some(isbn) = crate::resolver::cards::isbn(&format!("isbn:{asin}")) {
            doc.attributes.insert("isbn".into(), isbn);
        }
    }
    Some(doc)
}
/// Amazon's shops (without `www.`).
const AMAZON: &[&str] = &[
    "amazon.com",
    "amazon.ca",
    "amazon.com.mx",
    "amazon.com.br",
    "amazon.co.uk",
    "amazon.de",
    "amazon.fr",
    "amazon.it",
    "amazon.es",
    "amazon.nl",
    "amazon.se",
    "amazon.pl",
    "amazon.com.be",
    "amazon.com.tr",
    "amazon.ae",
    "amazon.sa",
    "amazon.eg",
    "amazon.in",
    "amazon.co.jp",
    "amazon.sg",
    "amazon.com.au",
    "amzn.to",
    "amzn.eu",
    "a.co",
];

fn first(v: &Value) -> &Value {
    v.as_array().and_then(|a| a.first()).unwrap_or(v)
}
fn all(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array()
        .map_or_else(|| vec![v], |a| a.iter().take(64).collect())
        .into_iter()
        .filter(|v| !v.is_null())
}
fn text(v: &Value) -> Option<String> {
    crate::event::page::string(v)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| {
            v.as_str()
                .and_then(|s| s.trim().replace(',', "").parse().ok())
        })
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn amount(n: f64) -> String {
    if n.fract() == 0.0 && n < 1e15 {
        format!("{}", n as u64)
    } else {
        format!("{:.2}", n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(s: &str) -> Url {
        s.parse().unwrap()
    }

    #[test]
    fn a_product_and_its_offer() {
        let p = from_jsonld(
            &json!({"@type": "Product", "name": "Nutella 400 g", "gtin13": "3017620422003",
                "brand": {"@type": "Brand", "name": "Ferrero"}, "sku": "123", "mpn": "N-400",
                "offers": [{"@type": "Offer", "price": "4.95", "priceCurrency": "chf",
                    "availability": "https://schema.org/InStock", "seller": {"name": "Shop"}},
                    {"@type": "Offer", "price": 3.5, "priceCurrency": "CHF"}]}),
            &page("https://shop.example/p/1"),
        );
        assert_eq!(p.gtin.as_deref(), Some("3017620422003"));
        assert_eq!(p.brand.as_deref(), Some("Ferrero"));
        assert_eq!(
            (p.sku.as_deref(), p.mpn.as_deref()),
            (Some("123"), Some("N-400"))
        );
        assert_eq!(p.price, Some(3.5));
        assert_eq!(p.currency.as_deref(), Some("CHF"));
        assert_eq!(p.availability.as_deref(), Some("in_stock"));
        assert_eq!(p.seller.as_deref(), Some("Shop"));
        let mut doc = ResourceMeta::new("https://shop.example/p/1", "jsonld", 3600);
        doc.set_type("Product");
        p.apply(&mut doc);
        let a = &doc.attributes;
        assert_eq!(a["price"], "3.50");
        assert_eq!(a["price_currency"], "CHF");
        assert!(a["price_at"].ends_with('Z'));
    }

    #[test]
    fn barcodes_in_their_spellings() {
        for (v, expected) in [
            (json!({"gtin": "0602557656527"}), Some("0602557656527")),
            (json!({"gtin12": "602557656527"}), Some("0602557656527")),
            (json!({"gtin14": "00602557656527"}), Some("0602557656527")),
            (json!({"gtin8": "96385074"}), Some("0000096385074")),
            (
                json!({"gtin": "https://id.gs1.org/01/03017620422003"}),
                Some("3017620422003"),
            ),
            (json!({"gtin13": 3017620422003u64}), Some("3017620422003")),
            (json!({"isbn": "0-14-032872-6"}), Some("9780140328721")),
            // A wrong check digit is no barcode.
            (json!({"gtin13": "3017620422004"}), None),
        ] {
            assert_eq!(gtin(&v).as_deref(), expected, "{v}");
        }
        assert_eq!(availability("OutOfStock").as_deref(), Some("out_of_stock"));
        assert_eq!(
            availability("http://schema.org/PreOrder").as_deref(),
            Some("pre_order")
        );
        assert_eq!(availability("soon"), None);
    }

    #[test]
    fn a_product_group_names_the_variant_its_page_is() {
        let group = json!({"@type": "ProductGroup", "name": "Runner", "brand": "Birds",
            "hasVariant": [
                {"@type": "Product", "url": "/products/runner?variant=1", "gtin": "0602557656527",
                    "offers": {"price": "100", "priceCurrency": "USD"}},
                {"@type": "Product", "url": "/products/runner?variant=2", "gtin": "4006381333931"}]});
        let p = from_jsonld(
            &group,
            &page("https://shop.example/products/runner?variant=2"),
        );
        assert_eq!(p.gtin.as_deref(), Some("4006381333931"));
        assert_eq!(p.brand.as_deref(), Some("Birds"));
        let p = from_jsonld(&group, &page("https://shop.example/products/runner"));
        assert_eq!(p.gtin, None, "no variant named, no barcode");
        // Different barcodes on one product's offers name no one product.
        let p = from_jsonld(
            &json!({"@type": "Product", "offers": [{"gtin": "0602557656527"}, {"gtin": "4006381333931"}]}),
            &page("https://shop.example/p"),
        );
        assert_eq!(p.gtin, None);
    }

    #[test]
    fn amazon_is_read_from_the_link() {
        let doc = amazon(
            "https://www.amazon.com/Matilda-Roald-Dahl/dp/0140328726/ref=sr_1_1",
            60,
        )
        .unwrap();
        assert_eq!(doc.type_name, "Product");
        assert_eq!(doc.site, "Amazon");
        assert_eq!(doc.attributes["asin"], "0140328726");
        assert_eq!(doc.attributes["isbn"], "0140328726");
        let doc = amazon("https://amazon.de/gp/product/B08N5WRWNW", 60).unwrap();
        assert_eq!(doc.attributes["asin"], "B08N5WRWNW");
        assert!(!doc.attributes.contains_key("isbn"));
        let doc = amazon("https://www.amazon.co.uk/gp/help/customer", 60).unwrap();
        assert_eq!(doc.type_name, "WebPage");
        assert!(amazon("https://www.amazonia.example/dp/B08N5WRWNW", 60).is_none());
        assert!(amazon("https://music.amazon.com/albums/B0", 60).is_none());
    }
}
