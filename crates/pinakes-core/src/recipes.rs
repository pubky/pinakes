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

/// Add every recipe to the public document as named examples. Body recipes become request body
/// examples. Parameter recipes become examples on each parameter of their operation, under the
/// same name: Swagger UI fills every parameter from the first recipe, and picking another
/// recipe's name in each dropdown switches to it; a parameter that recipe does not use gets an
/// empty value, which Swagger UI leaves out of the request.
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
        let used: Vec<&str> = recipes
            .iter()
            .flat_map(|r| r.params.keys().chain(r.query.keys()))
            .map(String::as_str)
            .collect();
        // `get_mut`, not indexing: indexing would insert `"parameters": null`.
        let Some(parameters) = operation
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for parameter in parameters {
            let name = parameter["name"].as_str().unwrap_or_default().to_owned();
            if !used.contains(&name.as_str()) {
                continue;
            }
            let integer = parameter["schema"]["type"] == "integer"
                || parameter["schema"]["type"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|t| t == "integer"));
            let mut examples = serde_json::Map::new();
            for recipe in recipes
                .iter()
                .filter(|r| r.body.is_none() || !r.query.is_empty())
            {
                let value = recipe.params.get(&name).or_else(|| recipe.query.get(&name));
                let value = match value {
                    Some(v) if integer => v.parse::<i64>().map(Value::from).expect("integer"),
                    Some(v) => Value::from(v.as_str()),
                    None => Value::from(""),
                };
                examples.insert(
                    recipe.name.clone(),
                    json!({"summary": recipe.summary, "value": value}),
                );
            }
            parameter["examples"] = Value::Object(examples);
        }
        if recipes.iter().filter(|r| r.body.is_none()).count() > 1 {
            let note = "Examples: pick the same example in each parameter's dropdown; \
                        parameters the example does not use stay empty.";
            let description = operation["description"].as_str().unwrap_or_default();
            operation["description"] = Value::from(if description.is_empty() {
                note.to_owned()
            } else {
                format!("{description}\n\n{note}")
            });
        }
    }
}
