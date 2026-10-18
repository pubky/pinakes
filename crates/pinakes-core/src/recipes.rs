//! Named OpenAPI examples for pubky.app features, from `data/api-recipes.json`. The public
//! document carries them as Swagger UI example dropdowns; `tests/recipes.rs` runs each one and
//! checks that docs/api.md lists it.
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize)]
pub struct Recipe {
    /// Example key in the document, unique.
    pub name: String,
    /// The dropdown label, in pubky.app terms.
    pub summary: String,
    /// What the feature does with the response.
    pub feature: String,
    pub method: String,
    /// The OpenAPI path template, e.g. `/v1/resources/{id}`.
    pub path: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    pub body: Option<Value>,
    /// What a working instance returns: `card`, `items`, `map`, `group` or `lines`.
    pub expect: String,
}

#[derive(Deserialize)]
struct File {
    recipes: Vec<Recipe>,
}

/// Every recipe, with `id(URI)` and `group(URI,...)` expanded to IDs.
pub fn recipes() -> Vec<Recipe> {
    let file: File = serde_json::from_str(include_str!("../../../data/api-recipes.json"))
        .expect("data/api-recipes.json");
    file.recipes
        .into_iter()
        .map(|mut r| {
            r.params.values_mut().for_each(|v| *v = expand(v));
            r.query.values_mut().for_each(|v| *v = expand(v));
            if let Some(body) = &mut r.body {
                expand_value(body);
            }
            r
        })
        .collect()
}

fn expand(value: &str) -> String {
    let id = |uri: &str| {
        crate::model::identity(uri.trim())
            .unwrap_or_else(|e| panic!("recipe URI {uri}: {e}"))
            .0
    };
    if let Some(uri) = value.strip_prefix("id(").and_then(|v| v.strip_suffix(')')) {
        id(uri)
    } else if let Some(members) = value
        .strip_prefix("group(")
        .and_then(|v| v.strip_suffix(')'))
    {
        // A group is named after its smallest member unit when it first forms (groups.rs).
        let first = members.split(',').map(id).min().expect("group members");
        pubky_resource_id::resource_id(&format!("group:auto:{first}"))
    } else {
        value.to_owned()
    }
}

fn expand_value(value: &mut Value) {
    match value {
        Value::String(s) => *s = expand(s),
        Value::Array(items) => items.iter_mut().for_each(expand_value),
        Value::Object(map) => map.values_mut().for_each(expand_value),
        _ => {}
    }
}

impl Recipe {
    /// The path with its parameters filled in, plus the query string.
    pub fn target(&self) -> String {
        let mut path = self.path.clone();
        for (name, value) in &self.params {
            path = path.replace(&format!("{{{name}}}"), value);
        }
        if self.query.is_empty() {
            return path;
        }
        let query = self
            .query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{path}?{query}")
    }
    /// The request as docs/api.md shows it, relative to the base URL.
    pub fn request_line(&self) -> String {
        match &self.body {
            Some(body) => format!("{} {} {body}", self.method, self.target()),
            None => format!("{} {}", self.method, self.target()),
        }
    }
}

/// The empty first example of a parameter that not every recipe of its operation uses.
pub const NONE: &str = "(none)";

/// Add every recipe to the public document as named examples. Body recipes become request body
/// examples. Parameter recipes become examples on the parameters they use, under the recipe's
/// name. The document's example maps are serde_json maps, whose keys serialize sorted, and Swagger
/// UI fills each parameter from the first example it lists; a parameter that not every recipe of
/// its operation uses therefore starts with an empty `(none)` example (`(` sorts before letters),
/// which Swagger UI leaves out of the request, so picking a recipe in the dropdowns of the
/// parameters it uses sends exactly that recipe. Panics on a recipe for an unknown operation, or
/// with a key that names no parameter of its operation in the right place (`params` are path
/// parameters, `query` keys query parameters).
pub fn apply(doc: &mut Value) {
    let mut by_operation = BTreeMap::<(String, String), Vec<Recipe>>::new();
    for recipe in recipes() {
        by_operation
            .entry((recipe.path.clone(), recipe.method.to_ascii_lowercase()))
            .or_default()
            .push(recipe);
    }
    for ((path, method), recipes) in by_operation {
        let operation = &mut doc["paths"][&path][&method];
        assert!(
            operation.is_object(),
            "recipe for unknown operation {method} {path}"
        );
        for recipe in recipes.iter().filter(|r| r.body.is_some()) {
            operation["requestBody"]["content"]["application/json"]["examples"][&recipe.name] = json!({
                "summary": recipe.summary,
                "description": recipe.feature,
                "value": recipe.body,
            });
        }
        // `get_mut`, not indexing: indexing would insert `"parameters": null`.
        let mut empty = Vec::new();
        let parameters = operation
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
            .unwrap_or(&mut empty);
        let names: Vec<(String, String)> = parameters
            .iter()
            .filter_map(|p| Some((p["name"].as_str()?.to_owned(), p["in"].as_str()?.to_owned())))
            .collect();
        for recipe in &recipes {
            let keys = recipe
                .params
                .keys()
                .map(|k| (k, "path"))
                .chain(recipe.query.keys().map(|k| (k, "query")));
            for (key, place) in keys {
                assert!(
                    names.iter().any(|(n, i)| n == key && i == place),
                    "recipe {}: `{key}` is not a {place} parameter of {method} {path}",
                    recipe.name
                );
            }
        }
        let parameter_recipes: Vec<&Recipe> = recipes
            .iter()
            .filter(|r| !r.params.is_empty() || !r.query.is_empty())
            .collect();
        let mut optional = false;
        for parameter in parameters.iter_mut() {
            let name = parameter["name"].as_str().unwrap_or_default().to_owned();
            let value = |r: &Recipe| r.params.get(&name).or_else(|| r.query.get(&name)).cloned();
            let using: Vec<&Recipe> = parameter_recipes
                .iter()
                .copied()
                .filter(|r| value(r).is_some())
                .collect();
            if using.is_empty() {
                continue;
            }
            let integer = parameter["schema"]["type"] == "integer"
                || parameter["schema"]["type"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|t| t == "integer"));
            let mut examples = serde_json::Map::new();
            if using.len() < parameter_recipes.len() {
                optional = true;
                examples.insert(
                    NONE.to_owned(),
                    json!({"summary": "(none): not used by the example", "value": ""}),
                );
            }
            for recipe in using {
                let v = value(recipe).expect("used");
                let v = if integer {
                    Value::from(v.parse::<i64>().expect("integer recipe value"))
                } else {
                    Value::from(v)
                };
                examples.insert(
                    recipe.name.clone(),
                    json!({"summary": recipe.summary, "value": v}),
                );
            }
            parameter["examples"] = Value::Object(examples);
        }
        if optional {
            let note = "Examples: pick the example in the dropdown of each parameter it uses; \
                        the other parameters stay (none) and are not sent.";
            let description = operation["description"].as_str().unwrap_or_default();
            operation["description"] = Value::from(if description.is_empty() {
                note.to_owned()
            } else {
                format!("{description}\n\n{note}")
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_keys_name_parameters_of_their_operation() {
        let mut doc = serde_json::to_value(<crate::api::ApiDoc as utoipa::OpenApi>::openapi())
            .expect("document");
        apply(&mut doc);
        let search = &doc["paths"]["/v1/search"]["get"]["parameters"];
        let q = search
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "q")
            .unwrap();
        let keys: Vec<&String> = q["examples"].as_object().unwrap().keys().collect();
        assert_eq!(keys, [NONE, "search-box"], "(none) first, then the users");
        let mut broken = serde_json::to_value(<crate::api::ApiDoc as utoipa::OpenApi>::openapi())
            .expect("document");
        broken["paths"]["/v1/search"]["get"]["parameters"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p["name"] != "q");
        let refused = std::panic::catch_unwind(move || apply(&mut broken))
            .expect_err("a recipe key without a parameter must fail");
        let message = refused
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("`q` is not a query parameter of get /v1/search"),
            "{message}"
        );
        // A path key placed under `query` is refused too (round 5 S15).
        let mut misplaced =
            serde_json::to_value(<crate::api::ApiDoc as utoipa::OpenApi>::openapi())
                .expect("document");
        for p in misplaced["paths"]["/v1/resources/{id}"]["get"]["parameters"]
            .as_array_mut()
            .unwrap()
        {
            if p["name"] == "id" {
                p["in"] = "query".into();
            }
        }
        let refused = std::panic::catch_unwind(move || apply(&mut misplaced))
            .expect_err("a path key under query must fail");
        let message = refused
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("`id` is not a path parameter"),
            "{message}"
        );
    }
}
