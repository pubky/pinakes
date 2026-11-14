//! Products on the Open Food Facts databases (Open Food Facts, Open Beauty Facts, Open Products
//! Facts, Open Pet Food Facts), read from their API by barcode: their product pages carry only
//! the site's own structured data. No key; the databases' data is under the ODbL, credited on
//! every card that shows it.
//!
//! Its also-on half ([`also_on::OpenFoodFacts`]) finds the product a card's barcode names on the
//! databases.
use super::{Budget, Context, Example, Source};
use crate::{
    kinds::products::{self, Product},
    model::ResourceMeta,
    resolver::egress::FetchOptions,
};
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde_json::Value;

pub struct OpenFoodFacts;

/// One of the databases: the product type it holds, its host and its name.
pub struct Database {
    pub product_type: &'static str,
    pub host: &'static str,
    pub site: &'static str,
}
pub const DATABASES: &[Database] = &[
    Database {
        product_type: "food",
        host: "openfoodfacts.org",
        site: "Open Food Facts",
    },
    Database {
        product_type: "beauty",
        host: "openbeautyfacts.org",
        site: "Open Beauty Facts",
    },
    Database {
        product_type: "product",
        host: "openproductsfacts.org",
        site: "Open Products Facts",
    },
    Database {
        product_type: "petfood",
        host: "openpetfoodfacts.org",
        site: "Open Pet Food Facts",
    },
];
/// The fields asked for (the databases answer every field otherwise).
const FIELDS: &str =
    "code,product_name,brands,quantity,product_type,nutriscore_grade,image_front_small_url";

impl Database {
    /// The API address of a barcode (GTIN-13).
    pub fn api(&self, code: &str) -> String {
        format!(
            "https://world.{}/api/v2/product/{code}.json?fields={FIELDS}",
            self.host
        )
    }
    /// The credit its data carries.
    pub fn credit(&self) -> String {
        format!("© {} contributors, ODbL 1.0", self.site)
    }
}

/// The database and barcode of a product page on one of the databases, and the barcode as the
/// page spells it: `https://{country or world}.{database}/product/{code}[/{slug}]` (`/produit/`
/// in French).
pub fn of_uri(uri: &str) -> Option<(&'static Database, String, String)> {
    let url = super::web_url(uri)?;
    let host = url.host_str()?.to_ascii_lowercase();
    let database = DATABASES.iter().find(|d| {
        host == d.host
            || host.strip_suffix(d.host).is_some_and(|sub| {
                sub.strip_suffix('.')
                    .is_some_and(|s| !s.is_empty() && !s.contains('.'))
            })
    })?;
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["product" | "produit", code, ..] => {
            Some((database, crate::barcode::gtin13(code)?, (*code).to_owned()))
        }
        _ => None,
    }
}
pub fn root(database: &Database, spelled: &str) -> String {
    format!("https://world.{}/product/{spelled}", database.host)
}

/// What a database answered for a barcode.
pub enum Answer {
    Product(Value),
    /// It holds the barcode as another type of product, another database's.
    Elsewhere(&'static Database),
    None,
}
/// A database's answer: its product of the barcode `code` (its own spelling may drop leading
/// zeros), the database it names for another type of product, or nothing.
pub fn answer(code: &str, status: u16, body: &[u8]) -> Result<Answer> {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) if status == 404 => return Ok(Answer::None),
        Err(e) => return Err(e.into()),
    };
    if status == 200 && v["status"] == 1 {
        let product = &v["product"];
        let same = product["code"]
            .as_str()
            .and_then(crate::barcode::gtin13)
            .is_some_and(|c| c == code);
        return Ok(if same {
            Answer::Product(product.clone())
        } else {
            Answer::None
        });
    }
    anyhow::ensure!(status == 404, "the database answered {status}");
    // "product found with a different product type: beauty".
    Ok(v["status_verbose"]
        .as_str()
        .and_then(|s| s.strip_prefix("product found with a different product type: "))
        .and_then(|t| DATABASES.iter().find(|d| d.product_type == t.trim()))
        .map_or(Answer::None, Answer::Elsewhere))
}
fn text<'a>(product: &'a Value, k: &str) -> Option<&'a str> {
    product[k]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 256)
}
/// The facts a database's product gives: its name, brands, quantity and (for food) Nutri-Score.
pub fn facts(product: &Value) -> Vec<(&'static str, String)> {
    [
        ("product_name", text(product, "product_name")),
        ("brands", text(product, "brands")),
        ("quantity", text(product, "quantity")),
        (
            "nutriscore",
            text(product, "nutriscore_grade").filter(|g| matches!(*g, "a" | "b" | "c" | "d" | "e")),
        ),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| (k, v.to_owned())))
    .collect()
}

/// The card of a database's product (`doc` for its page), from `database`'s record.
pub fn card(
    mut doc: ResourceMeta,
    database: &Database,
    code: &str,
    product: &Value,
) -> ResourceMeta {
    doc.set_type(products::TYPE);
    doc.site = database.site.into();
    doc.name = text(product, "product_name")
        .or_else(|| text(product, "brands"))
        .map(str::to_owned);
    doc.image = text(product, "image_front_small_url")
        .filter(|u| super::web_url(u).is_some())
        .map(str::to_owned);
    doc.attribution = Some(database.credit());
    Product {
        gtin: Some(code.to_owned()),
        brand: text(product, "brands").map(str::to_owned),
        ..Default::default()
    }
    .apply(&mut doc);
    for (k, v) in facts(product) {
        if matches!(k, "quantity" | "nutriscore") {
            doc.attributes.insert(k.into(), v);
        }
    }
    doc
}

#[async_trait]
impl Source for OpenFoodFacts {
    fn name(&self) -> &'static str {
        "open_food_facts"
    }
    fn reads(&self, uri: &str) -> bool {
        of_uri(uri).is_some()
    }
    fn alias_root(&self, uri: &str) -> Option<String> {
        let (database, _, spelled) = of_uri(uri)?;
        let root = root(database, &spelled);
        let (_, normalized, _) = crate::model::identity(uri).ok()?;
        (root != normalized).then_some(root)
    }
    fn facts(&self) -> &'static [&'static str] {
        &["gtin", "brand", "quantity", "nutriscore"]
    }
    fn budgets(&self) -> &'static [Budget] {
        // The databases' published limit: 100 product reads a minute.
        &[
            ("world.openfoodfacts.org", 1.66),
            ("world.openbeautyfacts.org", 1.66),
            ("world.openproductsfacts.org", 1.66),
            ("world.openpetfoodfacts.org", 1.66),
        ]
    }
    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                uri: "https://world.openfoodfacts.org/product/3017620422003/nutella-ferrero",
                card_type: "Product",
            },
            Example {
                uri: "https://world.openbeautyfacts.org/product/80466468",
                card_type: "Product",
            },
        ]
    }
    fn e2e(&self) -> Option<super::E2e> {
        use super::{E2e, E2eAnswer, E2eSample};
        Some(E2e {
            samples: &[E2eSample {
                kind: "open_food_facts",
                uri: "https://world.openfoodfacts.org/product/3017620422003/nutella-ferrero",
                card_type: "Product",
                group_with: None,
            }],
            answers: &[E2eAnswer {
                host: "world.openfoodfacts.org",
                path: "/api/v2/product/3017620422003.json",
                query: &[],
                header: None,
                content_type: "application/json",
                file: "off-3017620422003.json",
            }],
        })
    }
    async fn resolve(&self, cx: &Context<'_>, uri: &str) -> Result<ResourceMeta> {
        let (mut database, code, _) = of_uri(uri).context("not a product page")?;
        let options = FetchOptions::json();
        let f = cx.fetch(&database.api(&code), &options).await?;
        let mut found = answer(&code, f.status, &f.body)?;
        // Its page redirects to the database that holds it.
        if let Answer::Elsewhere(other) = found {
            database = other;
            let f = cx.fetch(&database.api(&code), &options).await?;
            found = answer(&code, f.status, &f.body)?;
        }
        Ok(match found {
            Answer::Product(product) => card(cx.card(uri, self), database, &code, &product),
            _ => cx.missing(uri, self),
        })
    }
}

pub mod also_on {
    //! The `open_food_facts` also-on source: a product card's barcode on the databases.
    use super::{answer, facts, of_uri, Answer, DATABASES};
    use crate::{
        also_on::{Context, Fact, Found, Link, Source, Subject},
        resolver::egress::FetchOptions,
    };
    use anyhow::Result;
    use async_trait::async_trait;

    /// The database's product of the barcode a product card states (any card but a database's
    /// own page): a derived link keyed by the barcode, and its facts with their credit.
    pub struct OpenFoodFacts;

    /// A product card's barcode, unless the card is a database's own page.
    fn barcode(subject: &Subject) -> Option<String> {
        if of_uri(&subject.uri).is_some() {
            return None;
        }
        let doc = subject.doc.as_ref()?;
        if !crate::kinds::products::is_product(doc) {
            return None;
        }
        crate::barcode::gtin13(doc.attributes.get("gtin")?)
    }

    #[async_trait]
    impl Source for OpenFoodFacts {
        fn id(&self) -> &'static str {
            "open_food_facts"
        }
        fn key_forms(&self) -> &'static [(&'static str, &'static str)] {
            &[("gtin", "3017620422003")]
        }
        fn applies(&self, subject: &Subject) -> bool {
            barcode(subject).is_some()
        }
        fn service_name(&self, _host: &str, url: &url::Url) -> Option<&'static str> {
            of_uri(url.as_str()).map(|(d, _, _)| d.site)
        }
        async fn lookup(&self, cx: &Context, subject: &Subject) -> Result<Found> {
            let mut found = Found::default();
            let Some(code) = barcode(subject) else {
                return Ok(found);
            };
            let options = FetchOptions::json();
            // Open Food Facts first: it names the database of a barcode it holds as another type
            // of product, which is then asked once.
            let mut database = &DATABASES[0];
            let mut asked = None;
            for _ in 0..2 {
                let f = cx
                    .egress
                    .discover(&database.api(&code), &options, 86400)
                    .await?;
                if f.status == 429 || f.status >= 500 {
                    return Err(crate::resolver::RetryLater {
                        class: "open_food_facts_busy",
                        seconds: f.retry_after_secs.clamp(60, 3600),
                    }
                    .into());
                }
                match answer(&code, f.status, &f.body)? {
                    Answer::Elsewhere(other) if asked.is_none() => {
                        asked = Some(database);
                        database = other;
                    }
                    Answer::Product(product) => {
                        let spelled = product["code"].as_str().unwrap_or(&code).to_owned();
                        found.links.push(
                            Link::new(
                                super::root(database, &spelled),
                                database.site,
                                "open_food_facts",
                            )
                            .keyed(format!("gtin:{code}")),
                        );
                        for (name, value) in facts(&product) {
                            found.facts.push(Fact::new(name, value));
                        }
                        found
                            .facts
                            .push(Fact::new("attribution", database.credit()));
                        break;
                    }
                    _ => break,
                }
            }
            Ok(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(file: &str) -> Vec<u8> {
        crate::sources::fixture::read("open_food_facts", file)
    }

    #[test]
    fn addresses() {
        for (uri, site, code, root) in [
            (
                "https://ch.openfoodfacts.org/product/3017620422003/nutella-ferrero",
                "Open Food Facts",
                "3017620422003",
                "https://world.openfoodfacts.org/product/3017620422003",
            ),
            (
                "https://fr.openfoodfacts.org/produit/3017620422003",
                "Open Food Facts",
                "3017620422003",
                "https://world.openfoodfacts.org/product/3017620422003",
            ),
            (
                "https://world.openbeautyfacts.org/product/80466468/dove",
                "Open Beauty Facts",
                "0000080466468",
                "https://world.openbeautyfacts.org/product/80466468",
            ),
        ] {
            let (d, c, _) = of_uri(uri).unwrap_or_else(|| panic!("{uri}"));
            assert_eq!((d.site, c.as_str()), (site, code), "{uri}");
            assert_eq!(
                OpenFoodFacts.alias_root(uri).as_deref(),
                Some(root),
                "{uri}"
            );
        }
        assert_eq!(
            OpenFoodFacts.alias_root("https://world.openfoodfacts.org/product/3017620422003"),
            None
        );
        for uri in [
            "https://world.openfoodfacts.org/category/spreads",
            "https://world.openfoodfacts.org/product/3017620422004",
            "https://openfoodfacts.org.example/product/3017620422003",
            "https://a.b.openfoodfacts.org/product/3017620422003",
        ] {
            assert!(!OpenFoodFacts.reads(uri), "{uri}");
        }
    }

    #[test]
    fn answers_and_cards() {
        let Answer::Product(product) =
            answer("3017620422003", 200, &read("off-3017620422003.json")).unwrap()
        else {
            panic!("a product");
        };
        let uri = "https://world.openfoodfacts.org/product/3017620422003";
        let doc = card(
            ResourceMeta::new(uri, "open_food_facts", 3600),
            &DATABASES[0],
            "3017620422003",
            &product,
        );
        assert_eq!(doc.type_name, "Product");
        assert_eq!(doc.name.as_deref(), Some("Nutella"));
        assert_eq!(doc.site, "Open Food Facts");
        assert!(doc
            .image
            .as_deref()
            .unwrap()
            .starts_with("https://images.openfoodfacts.org/"));
        assert_eq!(
            doc.attribution.as_deref(),
            Some("© Open Food Facts contributors, ODbL 1.0")
        );
        let a = &doc.attributes;
        assert_eq!(a["gtin"], "3017620422003");
        assert_eq!(a["brand"], "Nutella, Ferrero");
        assert_eq!(a["quantity"], "400 g e");
        assert_eq!(a["nutriscore"], "e");
        assert!(doc.clone().bounded(crate::sources::attributes()).is_ok());

        // Another type of product: its database.
        assert!(matches!(
            answer("0000080466468", 404, &read("off-0000080466468.json")).unwrap(),
            Answer::Elsewhere(d) if d.site == "Open Beauty Facts"
        ));
        assert!(matches!(
            answer("6111259733749", 404, &read("off-6111259733749.json")).unwrap(),
            Answer::Elsewhere(d) if d.site == "Open Products Facts"
        ));
        // The database's spelling drops the leading zeros of an EAN-8.
        assert!(matches!(
            answer("0000080466468", 200, &read("obf-0000080466468.json")).unwrap(),
            Answer::Product(p) if p["code"] == "80466468"
        ));
        assert!(matches!(
            answer("0602557656527", 404, &read("off-0602557656527.json")).unwrap(),
            Answer::None
        ));
        // A record of another barcode is none of this one.
        assert!(matches!(
            answer("0602557656527", 200, &read("off-3017620422003.json")).unwrap(),
            Answer::None
        ));
        assert!(answer("3017620422003", 503, b"busy").is_err());
    }
}
