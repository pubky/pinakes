//! Products (`Product` and its subtypes): the barcode and what a shop says about one, from the
//! page's own JSON-LD or the shop's own product record (a page hook in `sources`).
//!
//! The barcode (`gtin`) is the product's GTIN-13 (`barcode::gtin13`), from the product's own
//! `gtin`, `gtin8`, `gtin12`, `gtin13`, `gtin14` or `isbn`, never from text. The also-on
//! sources make it the key `gtin:` that groups the same product on several shops. A price and an
//! availability are facts with the time they were read (`price_at`), never identity.
//!
//! Amazon pages are never fetched (the egress refuses them, a redirect's target too): an Amazon
//! product link's card comes from the link alone (`amazon`).
use super::{first, number, text_or_number as text};
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
    // The price of the page's cheapest offer, and that one offer's ISO 4217 currency,
    // availability (`in_stock`, `out_of_stock`, `pre_order`, `back_order`, `discontinued`,
    // `limited_availability`, `sold_out`, `in_store_only`, `online_only`) and seller; when they
    // were read (RFC 3339).
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
    /// variant's) than the page's. The record's identity (barcode, article and part number) is
    /// all there is of it: one it lacks, or gives wrong, is not taken from another variant the
    /// page names. An offer is one: its price, currency, time, availability and seller go
    /// together.
    pub fn replace(self, doc: &mut ResourceMeta) {
        let offer = self.price.is_some() || self.availability.is_some();
        for (key, replaced) in [
            ("gtin", true),
            ("sku", true),
            ("mpn", true),
            ("brand", self.brand.is_some()),
            ("price", offer),
            ("price_currency", offer),
            ("price_at", offer),
            ("availability", offer),
            ("seller", offer),
        ] {
            if replaced {
                doc.attributes.remove(key);
            }
        }
        self.apply(doc);
    }
    /// This product without its identity (barcode, article and part number): what a product
    /// that is not the page's own says, which must not join the page to that product's group.
    pub fn without_identity(self) -> Product {
        Product {
            gtin: None,
            sku: None,
            mpn: None,
            ..self
        }
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

/// Remove a product's identity (barcode, article and part number) from `doc`: a page that
/// names several products of its own states none.
pub fn drop_identity(doc: &mut ResourceMeta) {
    for key in ["gtin", "sku", "mpn"] {
        doc.attributes.remove(key);
    }
}

/// The product a JSON-LD `Product` node describes on the page at `page`: for a `ProductGroup`,
/// the variant the page's address names (its `url` or `@id`, or Shopify's `?variant=`), else its
/// only variant. A variant's barcode, article numbers and offers are its own, never the group's
/// (the group's offers give its lowest price, or another variant's barcode).
pub fn from_jsonld(v: &Value, page: &Url) -> Product {
    let variant = variant(v, page);
    let node = variant.unwrap_or(v);
    let offers: Vec<&Value> = all(&node["offers"])
        .flat_map(|o| std::iter::once(o).chain(all(&o["offers"])))
        .collect();
    let gtin = gtin(node).or_else(|| {
        // The offer of this one product may carry its barcode instead.
        let codes: Vec<String> = offers.iter().filter_map(|o| gtin(o)).collect();
        (!codes.is_empty() && codes.iter().all(|c| *c == codes[0])).then(|| codes[0].clone())
    });
    // Price, currency, availability and seller come from one offer: the cheapest, a concrete
    // price before an aggregate's low price; without a price, the first naming an availability.
    let priced = |o: &Value| {
        number(&o["price"])
            .or_else(|| number(&o["priceSpecification"]["price"]))
            .map(|p| (p, false))
            .or_else(|| number(&o["lowPrice"]).map(|p| (p, true)))
    };
    let available = |o: &Value| text(&o["availability"]).and_then(|a| availability(&a));
    let cheapest = offers
        .iter()
        .copied()
        .filter_map(|o| priced(o).map(|p| (p, o)))
        .reduce(|a, b| if b.0 < a.0 { b } else { a });
    let (price, offer) = match cheapest {
        Some(((price, _), offer)) => (Some(price), Some(offer)),
        None => (
            None,
            offers.iter().copied().find(|o| available(o).is_some()),
        ),
    };
    let brand = first(&node["brand"]);
    let brand = text(brand).or_else(|| text(&brand["name"])).or_else(|| {
        let brand = first(&v["brand"]);
        text(brand).or_else(|| text(&brand["name"]))
    });
    Product {
        gtin,
        brand,
        sku: text(&node["sku"]),
        mpn: text(&node["mpn"]),
        price,
        currency: offer
            .filter(|_| price.is_some())
            .and_then(|o| {
                text(&o["priceCurrency"])
                    .or_else(|| text(&o["priceSpecification"]["priceCurrency"]))
            })
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())),
        availability: offer.and_then(available),
        seller: offer.and_then(|o| text(&o["seller"]["name"]).or_else(|| text(&o["seller"]))),
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
    if !on_amazon(&url) {
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
/// Whether `url` is on Amazon's shop, which is never fetched: the egress refuses it at every
/// step, a redirect's target too.
pub fn on_amazon(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        AMAZON.contains(&host.strip_prefix("www.").unwrap_or(&host))
    })
}
/// Amazon's shops (without `www.`) and its short links, as of 2026.
const AMAZON: &[&str] = &[
    "amazon.com",
    "amazon.ca",
    "amazon.com.mx",
    "amazon.com.br",
    "amazon.co.uk",
    "amazon.ie",
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
    "amazon.co.za",
    "amazon.in",
    "amazon.co.jp",
    "amazon.cn",
    "amazon.sg",
    "amazon.com.au",
    "amzn.to",
    "amzn.eu",
    "amzn.asia",
    "a.co",
];

fn all(v: &Value) -> impl Iterator<Item = &Value> {
    super::all(v, 64)
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
                "offers": [{"@type": "Offer", "price": "4.95", "priceCurrency": "CHF"},
                    {"@type": "Offer", "price": 3.5, "priceCurrency": "chf",
                    "availability": "https://schema.org/InStock", "seller": {"name": "Shop"}}]}),
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
    fn price_seller_and_availability_are_one_offers() {
        let p = from_jsonld(
            &json!({"@type": "Product", "offers": [
                {"@type": "Offer", "price": "100", "priceCurrency": "CHF",
                    "availability": "https://schema.org/OutOfStock",
                    "seller": {"name": "Expensive shop"}},
                {"@type": "Offer", "price": "50", "priceCurrency": "CHF",
                    "availability": "https://schema.org/InStock", "seller": {"name": "Cheap shop"}}]}),
            &page("https://shop.example/p/1"),
        );
        assert_eq!(p.price, Some(50.0));
        assert_eq!(p.availability.as_deref(), Some("in_stock"));
        assert_eq!(p.seller.as_deref(), Some("Cheap shop"));
        // An aggregate's low price gives way to the same price of a concrete offer, with its
        // seller; without a price, the offer naming an availability.
        let p = from_jsonld(
            &json!({"@type": "Product", "offers": {"@type": "AggregateOffer", "lowPrice": "50",
                "priceCurrency": "EUR", "offers": [
                    {"price": "70", "priceCurrency": "EUR", "seller": "A"},
                    {"price": "50", "priceCurrency": "EUR", "seller": "B"}]}}),
            &page("https://shop.example/p/1"),
        );
        assert_eq!(
            (p.price, p.currency.as_deref(), p.seller.as_deref()),
            (Some(50.0), Some("EUR"), Some("B"))
        );
        let p = from_jsonld(
            &json!({"@type": "Product", "offers": [{"seller": "A"},
                {"availability": "SoldOut", "seller": "B"}]}),
            &page("https://shop.example/p/1"),
        );
        assert_eq!(
            (p.price, p.availability.as_deref(), p.seller.as_deref()),
            (None, Some("sold_out"), Some("B"))
        );
    }

    #[test]
    fn a_variant_keeps_its_own_barcode_and_offers() {
        // The group's offers are its aggregate (the lowest price of any variant, and that
        // variant's barcode): the variant the page names takes neither, nor the group's sku.
        let group = json!({"@type": "ProductGroup", "name": "Runner", "sku": "RUNNER",
            "offers": {"@type": "AggregateOffer", "lowPrice": "10", "priceCurrency": "USD",
                "offers": [{"price": "10", "priceCurrency": "USD", "gtin": "0602557656527"}]},
            "hasVariant": [
                {"@type": "Product", "url": "/products/runner?variant=1", "gtin": "0602557656527",
                    "sku": "R-1", "offers": {"price": "10", "priceCurrency": "USD"}},
                {"@type": "Product", "url": "/products/runner?variant=2", "sku": "R-2",
                    "offers": {"price": "20", "priceCurrency": "USD"}}]});
        let p = from_jsonld(
            &group,
            &page("https://shop.example/products/runner?variant=2"),
        );
        assert_eq!(p.gtin, None);
        assert_eq!(p.sku.as_deref(), Some("R-2"));
        assert_eq!((p.price, p.currency.as_deref()), (Some(20.0), Some("USD")));
        // The group's own page: the group's facts.
        let p = from_jsonld(&group, &page("https://shop.example/products/runner"));
        assert_eq!((p.price, p.sku.as_deref()), (Some(10.0), Some("RUNNER")));
    }

    #[test]
    fn a_variants_record_replaces_the_pages_identity_and_offer() {
        let mut doc = ResourceMeta::new("https://shop.example/products/runner", "jsonld", 3600);
        doc.set_type("Product");
        Product {
            gtin: Some("0602557656527".into()),
            sku: Some("R-1".into()),
            mpn: Some("M-1".into()),
            brand: Some("Birds".into()),
            price: Some(10.0),
            currency: Some("USD".into()),
            availability: Some("in_stock".into()),
            seller: Some("Shop".into()),
            ..Default::default()
        }
        .apply(&mut doc);
        Product {
            sku: Some("R-2".into()),
            price: Some(20.0),
            currency: Some("USD".into()),
            ..Default::default()
        }
        .replace(&mut doc);
        let a = &doc.attributes;
        assert!(!a.contains_key("gtin") && !a.contains_key("mpn"), "{a:?}");
        assert_eq!(a["sku"], "R-2");
        assert_eq!(a["price"], "20");
        assert!(!a.contains_key("availability") && !a.contains_key("seller"));
        assert_eq!(a["brand"], "Birds");
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
