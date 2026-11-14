//! Recipes (`Recipe`), from a recipe page's own JSON-LD: its time, yield, cuisine, category, diets,
//! rating, and its ingredients without their amounts, which search's `ingredient` matches by word.
use super::text_or_number as text;
use crate::model::ResourceMeta;
use serde_json::Value;

pub const TYPE: &str = "Recipe";

/// The attributes a recipe publishes in `facts`.
pub const FACTS: &[&str] = &[
    // Minutes, from `totalTime` (else `prepTime` and `cookTime`).
    "total_minutes",
    // As the recipe gives it: "4", "6 servings".
    "recipe_yield",
    // Ingredients without amounts or units, lower case, joined with "; " (as many as 256 bytes
    // hold, in the recipe's order).
    "ingredients",
    "cuisine",
    "recipe_category",
    // `gluten_free`, `vegan`, `vegetarian`, … (schema.org's RestrictedDiet, in snake case).
    "diet",
    // The average rating and how many ratings it averages.
    "rating",
    "rating_count",
];

pub fn is_recipe(doc: &ResourceMeta) -> bool {
    doc.type_path.iter().any(|t| t == TYPE)
}

/// Record a JSON-LD `Recipe` node's facts on `doc`, keeping what it already holds.
pub fn apply_jsonld(v: &Value, doc: &mut ResourceMeta) {
    let minutes = |k: &str| {
        text(&v[k])
            .and_then(|d| crate::event::time::duration(&d))
            .map(|d| d.num_minutes())
            .filter(|m| (1..=60 * 24 * 14).contains(m))
    };
    let total = minutes("totalTime").or_else(|| match (minutes("prepTime"), minutes("cookTime")) {
        (None, None) => None,
        (prep, cook) => Some(prep.unwrap_or(0) + cook.unwrap_or(0)),
    });
    let ingredients: Vec<String> = all(&v["recipeIngredient"])
        .chain(all(&v["ingredients"]))
        .filter_map(text)
        .filter_map(|i| ingredient(&i))
        .collect();
    let rating = &v["aggregateRating"];
    let diets: Vec<String> = all(&v["suitableForDiet"])
        .filter_map(text)
        .filter_map(|d| diet(&d))
        .collect();
    for (key, value) in [
        ("total_minutes", total.map(|m| m.to_string())),
        ("recipe_yield", texts(&v["recipeYield"]).into_iter().next()),
        ("ingredients", joined(&ingredients)),
        ("cuisine", joined(&split_list(&v["recipeCuisine"]))),
        ("recipe_category", joined(&split_list(&v["recipeCategory"]))),
        ("diet", joined(&diets)),
        ("rating", number(&rating["ratingValue"])),
        (
            "rating_count",
            number(&rating["ratingCount"]).or_else(|| number(&rating["reviewCount"])),
        ),
    ] {
        if let Some(value) = value {
            doc.attributes.entry(key.into()).or_insert(value);
        }
    }
}

/// Units an amount may carry, as recipes write them (lower case, without a final `.`).
const UNITS: &[&str] = &[
    "cup",
    "cups",
    "c",
    "tbsp",
    "tbs",
    "tablespoon",
    "tablespoons",
    "tsp",
    "teaspoon",
    "teaspoons",
    "g",
    "gram",
    "grams",
    "kg",
    "mg",
    "ml",
    "l",
    "litre",
    "litres",
    "liter",
    "liters",
    "dl",
    "cl",
    "oz",
    "ounce",
    "ounces",
    "lb",
    "lbs",
    "pound",
    "pounds",
    "pinch",
    "pinches",
    "dash",
    "dashes",
    "clove",
    "cloves",
    "can",
    "cans",
    "tin",
    "tins",
    "pack",
    "packs",
    "package",
    "packages",
    "packet",
    "packets",
    "slice",
    "slices",
    "stick",
    "sticks",
    "bunch",
    "bunches",
    "handful",
    "handfuls",
    "ball",
    "balls",
    "sprig",
    "sprigs",
    "piece",
    "pieces",
    "jar",
    "jars",
    "bottle",
    "bottles",
    "el",
    "tl",
    "prise",
    "stk",
    "stück",
    "msp",
    "bund",
    "dose",
    "becher",
    "würfel",
];
/// Words after a comma that say how to prepare it, not what it is.
const PREPARATION: &[&str] = &[
    "beaten",
    "chopped",
    "diced",
    "minced",
    "sliced",
    "grated",
    "peeled",
    "divided",
    "softened",
    "melted",
    "cubed",
    "crushed",
    "drained",
    "rinsed",
    "halved",
    "quartered",
    "trimmed",
    "cut",
    "finely",
    "roughly",
    "thinly",
    "coarsely",
    "freshly",
    "to",
    "at",
    "or",
    "plus",
    "optional",
    "for",
    "about",
    "room",
    "cooked",
    "shredded",
    "torn",
    "zested",
    "juiced",
    "seeded",
    "sifted",
];

/// An ingredient line without its amount, unit, notes in parentheses and preparation:
/// "1 lb. boneless, skinless chicken breast ($6.25)" is "boneless, skinless chicken breast",
/// "3 large eggs, beaten" is "large eggs", "750g lean beef mince" is "lean beef mince".
pub fn ingredient(line: &str) -> Option<String> {
    let mut text = String::with_capacity(line.len());
    let mut depth = 0usize;
    for c in line.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => text.extend(c.to_lowercase()),
            _ => {}
        }
    }
    let amount = |w: &str| {
        let w = w.trim_end_matches('.');
        !w.is_empty()
            && w.chars().all(|c| {
                c.is_ascii_digit()
                    || matches!(
                        c,
                        '/' | '.' | ',' | '-' | '–' | '½' | '¼' | '¾' | '⅓' | '⅔' | '⅛'
                    )
            })
    };
    // An amount glued to its unit: "750g", "200ml".
    let glued = |w: &str| {
        let w = w.trim_end_matches('.');
        let unit =
            w.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '.' | ',' | '/'));
        unit.len() < w.len() && UNITS.contains(&unit)
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut start = 0;
    while start < words.len() {
        let w = words[start].trim_end_matches('.');
        // A unit word is one only before what it measures: "2 cloves garlic", "pinch salt";
        // the last word is the ingredient ("2 cloves", "1 tsp cloves").
        let unit = UNITS.contains(&w) && start + 1 < words.len();
        if amount(w) || glued(w) || unit || matches!(w, "x" | "of") {
            start += 1;
        } else {
            break;
        }
    }
    let mut rest = words[start..].join(" ");
    // "olive oil plus extra for the dish", "passata or half our basic tomato sauce".
    for alternative in [" plus ", " or "] {
        if let Some(at) = rest.find(alternative) {
            rest.truncate(at);
        }
    }
    // A preparation after a comma goes; a list of what it is ("boneless, skinless …") stays.
    let mut parts: Vec<&str> = rest.split(',').map(str::trim).collect();
    while parts.len() > 1
        && parts.last().is_some_and(|p| {
            p.split_whitespace()
                .next()
                .is_none_or(|w| PREPARATION.contains(&w))
        })
    {
        parts.pop();
    }
    let name = parts
        .join(", ")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_owned();
    (!name.is_empty() && name.chars().any(char::is_alphabetic) && name.len() <= 80).then_some(name)
}

/// A schema.org diet (`https://schema.org/GlutenFreeDiet`) in snake case (`gluten_free`).
pub fn diet(s: &str) -> Option<String> {
    let name = s.trim().rsplit(['/', ':']).next()?;
    let name = name.strip_suffix("Diet").unwrap_or(name);
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        if !c.is_ascii_alphabetic() {
            return None;
        }
        out.push(c.to_ascii_lowercase());
    }
    (!out.is_empty() && out.len() <= 32).then_some(out)
}

fn all(v: &Value) -> impl Iterator<Item = &Value> {
    super::all(v, 64)
}
fn texts(v: &Value) -> Vec<String> {
    all(v).filter_map(text).collect()
}
/// A list given as an array or as one comma-separated string.
fn split_list(v: &Value) -> Vec<String> {
    texts(v)
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .map(str::to_owned)
        .collect()
}
fn number(v: &Value) -> Option<String> {
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
    Some(if n.fract() == 0.0 {
        format!("{}", n as u64)
    } else {
        format!("{n}")
    })
}
/// Values joined with "; ", once each, as many whole ones as an attribute holds (256 bytes).
fn joined(values: &[String]) -> Option<String> {
    let mut out = String::new();
    let mut seen: Vec<&str> = vec![];
    for value in values {
        if seen.iter().any(|s| s.eq_ignore_ascii_case(value)) {
            continue;
        }
        let sep = if out.is_empty() { "" } else { "; " };
        if out.len() + sep.len() + value.len() > 256 {
            break;
        }
        seen.push(value);
        out.push_str(sep);
        out.push_str(value);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ingredients_without_their_amounts() {
        for (line, name) in [
            (
                "1 lb. boneless, skinless chicken breast ($6.25)",
                "boneless, skinless chicken breast",
            ),
            ("2 Tbsp butter ($0.28)", "butter"),
            ("2 cloves garlic ($0.16)", "garlic"),
            ("1/2 lb. penne pasta ($0.67)", "penne pasta"),
            ("1.5 cups chicken broth", "chicken broth"),
            ("3 large eggs, beaten", "large eggs"),
            ("750g lean beef mince", "lean beef mince"),
            ("200ml hot beef stock", "hot beef stock"),
            ("½ tsp salt, or to taste", "salt"),
            ("2 x 400g cans of chopped tomatoes", "chopped tomatoes"),
            ("nutmeg", "nutmeg"),
            ("2 olive oil plus extra for the dish", "olive oil"),
            ("800g passata or half our basic tomato sauce", "passata"),
            (
                "125g ball mozzarella torn into thin strips",
                "mozzarella torn into thin strips",
            ),
            (
                "white sauce (find a recipe in the method, or use shop-bought)",
                "white sauce",
            ),
            ("3 oz. cream cheese* ($0.80)", "cream cheese"),
            ("pinch salt", "salt"),
            ("500 g Mehl", "mehl"),
            // A unit word that ends the line is what it measures.
            ("2 cloves", "cloves"),
            ("1 tsp cloves", "cloves"),
            ("2 cloves garlic", "garlic"),
        ] {
            assert_eq!(ingredient(line).as_deref(), Some(name), "{line}");
        }
        assert_eq!(ingredient("2 (3 oz)"), None);
        assert_eq!(
            diet("https://schema.org/GlutenFreeDiet").as_deref(),
            Some("gluten_free")
        );
        assert_eq!(diet("VeganDiet").as_deref(), Some("vegan"));
        assert_eq!(
            diet("http://schema.org/LowCalorieDiet").as_deref(),
            Some("low_calorie")
        );
    }

    #[test]
    fn a_recipe_from_its_json_ld() {
        let mut doc = ResourceMeta::new("https://cook.example/r", "jsonld", 60);
        doc.set_type("Recipe");
        apply_jsonld(
            &json!({"@type": "Recipe", "name": "Classic lasagne", "prepTime": "PT30M",
                "cookTime": "PT1H30M", "recipeYield": 6, "recipeCuisine": "Italian",
                "recipeCategory": "Dinner, Lunch, Main course",
                "suitableForDiet": ["https://schema.org/LowLactoseDiet"],
                "aggregateRating": {"ratingValue": "4.84", "ratingCount": "254"},
                "recipeIngredient": ["750g lean beef mince", "2 garlic cloves, crushed", "750g lean beef mince"]}),
            &mut doc,
        );
        let a = &doc.attributes;
        assert_eq!(a["total_minutes"], "120");
        assert_eq!(a["recipe_yield"], "6");
        assert_eq!(a["ingredients"], "lean beef mince; garlic cloves");
        assert_eq!(a["cuisine"], "Italian");
        assert_eq!(a["recipe_category"], "Dinner; Lunch; Main course");
        assert_eq!(a["diet"], "low_lactose");
        assert_eq!((&*a["rating"], &*a["rating_count"]), ("4.84", "254"));
    }
}
