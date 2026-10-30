//! Shared test helpers: a small OpenAPI 3.1 response check, and an official source's card from
//! its recorded answers for a test that needs one source (and skips without it).
#![allow(dead_code)]
pub mod media;
use serde_json::Value;
use std::collections::BTreeSet;

/// A schema with its `$ref` followed; a dangling reference is an error and checks nothing.
fn resolve<'a>(doc: &'a Value, schema: &'a Value, at: &str, errors: &mut Vec<String>) -> &'a Value {
    match schema["$ref"].as_str() {
        Some(r) => {
            let target = r
                .strip_prefix("#/components/schemas/")
                .map(|name| &doc["components"]["schemas"][name]);
            match target.filter(|t| t.is_object()) {
                Some(t) => resolve(doc, t, at, errors),
                None => {
                    errors.push(format!("{at}: dangling $ref {r}"));
                    &Value::Null
                }
            }
        }
        None => schema,
    }
}
/// Every property name an object schema declares, through `allOf`.
fn declared(doc: &Value, schema: &Value, names: &mut BTreeSet<String>) -> bool {
    let schema = resolve(doc, schema, "", &mut vec![]);
    let mut any = false;
    if let Some(p) = schema["properties"].as_object() {
        names.extend(p.keys().cloned());
        any = true;
    }
    for part in schema["allOf"].as_array().into_iter().flatten() {
        any |= declared(doc, part, names);
    }
    any
}
/// `$ref`, `allOf`, `anyOf`, `oneOf` (exactly one option), types (with null), enums, required
/// and undeclared properties (absent or `false` `additionalProperties`), additional-property
/// schemas and array items.
pub fn conforms(doc: &Value, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    check(doc, schema, value, at, errors, true)
}
fn matches(doc: &Value, schema: &Value, value: &Value, at: &str) -> bool {
    let mut e = vec![];
    conforms(doc, schema, value, at, &mut e);
    e.is_empty()
}
fn check(
    doc: &Value,
    schema: &Value,
    value: &Value,
    at: &str,
    errors: &mut Vec<String>,
    whole: bool,
) {
    let schema = resolve(doc, schema, at, errors);
    // The parts of an `allOf` declare its properties together: only the whole checks them.
    for part in schema["allOf"].as_array().into_iter().flatten() {
        check(doc, part, value, at, errors, false);
    }
    if let Some(options) = schema["anyOf"].as_array() {
        if !options.iter().any(|o| matches(doc, o, value, at)) {
            errors.push(format!("{at}: matches no anyOf option: {value}"));
        }
    }
    if let Some(options) = schema["oneOf"].as_array() {
        let n = options
            .iter()
            .filter(|o| matches(doc, o, value, at))
            .count();
        if n != 1 {
            errors.push(format!("{at}: matches {n} oneOf options, not one: {value}"));
        }
    }
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(t) => t.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    if !types.is_empty()
        && !types.contains(&kind)
        && !(kind == "integer" && types.contains(&"number"))
    {
        errors.push(format!("{at}: {kind} where the schema says {types:?}"));
        return;
    }
    if let Some(options) = schema["enum"].as_array() {
        if !options.contains(value) {
            errors.push(format!("{at}: {value} is not one of {options:?}"));
        }
    }
    if let Some(object) = value.as_object() {
        for name in schema["required"].as_array().into_iter().flatten() {
            if !object.contains_key(name.as_str().unwrap_or_default()) {
                errors.push(format!("{at}: missing required {name}"));
            }
        }
        let mut names = BTreeSet::new();
        let extra = &schema["additionalProperties"];
        let has_properties = declared(doc, schema, &mut names);
        if whole && !has_properties && extra == &Value::Bool(false) {
            // A closed object that declares nothing allows nothing.
            for key in object.keys() {
                errors.push(format!("{at}: undeclared property {key}"));
            }
        } else if whole && has_properties {
            for key in object.keys().filter(|k| !names.contains(*k)) {
                match extra {
                    Value::Null | Value::Bool(false) => {
                        errors.push(format!("{at}: undeclared property {key}"))
                    }
                    Value::Object(_) => {
                        conforms(doc, extra, &object[key], &format!("{at}.{key}"), errors)
                    }
                    _ => {}
                }
            }
        } else if extra.is_object() {
            for (key, v) in object {
                conforms(doc, extra, v, &format!("{at}.{key}"), errors);
            }
        }
        for (key, property) in schema["properties"].as_object().into_iter().flatten() {
            if let Some(v) = object.get(key) {
                conforms(doc, property, v, &format!("{at}.{key}"), errors);
            }
        }
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (i, v) in values.iter().enumerate() {
            conforms(doc, items, v, &format!("{at}[{i}]"), errors);
        }
    }
}
/// The operation a document declares for `method path`, if any. Path templates match one
/// segment per `{parameter}`; the query string is ignored.
fn operation<'a>(doc: &'a Value, method: &str, path: &str) -> Option<&'a Value> {
    let path = path.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').collect();
    doc["paths"]
        .as_object()?
        .iter()
        .find_map(|(template, item)| {
            let parts: Vec<&str> = template.split('/').collect();
            let fits = parts.len() == segments.len()
                && parts.iter().zip(&segments).all(|(t, s)| {
                    (t.starts_with('{') && t.ends_with('}') && !s.is_empty()) || t == s
                });
            fits.then(|| &item[method.to_ascii_lowercase()])
                .filter(|op| op.is_object())
        })
}
/// The JSON schema a document declares for `method path` answering `status`, if any.
pub fn response_schema<'a>(
    doc: &'a Value,
    method: &str,
    path: &str,
    status: u16,
) -> Option<&'a Value> {
    let schema = &operation(doc, method, path)?["responses"][status.to_string()]["content"]
        ["application/json"]["schema"];
    schema.is_object().then_some(schema)
}
/// Check one JSON response against the first document that declares its operation. An
/// undeclared operation or status is an error, except the router's own 404/405 for paths no
/// document has and the statuses middleware answers on every route (429 budget, 500 backstop,
/// 503 busy).
pub fn validate(
    docs: &[&Value],
    method: &str,
    path: &str,
    status: u16,
    value: &Value,
) -> Vec<String> {
    let mut errors = vec![];
    if matches!(status, 429 | 500 | 503) {
        return errors;
    }
    let Some((doc, op)) = docs
        .iter()
        .find_map(|doc| operation(doc, method, path).map(|op| (*doc, op)))
    else {
        if !matches!(status, 404 | 405) {
            errors.push(format!("no document declares {method} {path}"));
        }
        return errors;
    };
    let response = &op["responses"][status.to_string()];
    if !response.is_object() {
        errors.push(format!("{method} {path} does not declare {status}"));
        return errors;
    }
    let schema = &response["content"]["application/json"]["schema"];
    if schema.is_object() {
        conforms(doc, schema, value, "$", &mut errors);
    }
    errors
}
/// The public document, built once per test binary.
pub fn document() -> &'static Value {
    static DOC: std::sync::LazyLock<Value> =
        std::sync::LazyLock::new(pinakes_core::api::openapi_document);
    &DOC
}
/// Assert that one public API response matches what the public document declares for it; an
/// admin-only operation is undeclared there.
pub fn check_response(method: &str, path: &str, status: u16, value: &Value) {
    let errors = validate(&[document()], method, path, status, value);
    assert!(
        errors.is_empty(),
        "{method} {path} {status}: {errors:#?}\n{value}"
    );
}

/// The card a registered official source makes of `uri` from recorded answers (`(url, status,
/// content type, file)`, the files in `tests/fixtures/sources/{name}/`), or `None` when this build
/// has no source named `name`: a test that needs one source returns early without it.
pub async fn source_card(
    name: &str,
    uri: &str,
    answers: &[(&str, u16, &str, &str)],
) -> Option<pinakes_core::model::ResourceMeta> {
    let source = pinakes_core::sources::by_name(name)?;
    let dir = format!(
        "{}/tests/fixtures/sources/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let routes = answers
        .iter()
        .map(|(url, status, kind, file)| {
            let body = std::fs::read(format!("{dir}/{file}")).unwrap();
            (url.to_string(), (*status, kind.to_string(), body))
        })
        .collect();
    let egress = pinakes_core::resolver::egress::Egress::with_backends(
        Default::default(),
        std::sync::Arc::new(PublicDns),
        std::sync::Arc::new(Recorded(routes)),
    );
    let cx = pinakes_core::sources::Context {
        egress: &egress,
        ttl: 3600,
    };
    Some(source.resolve(&cx, uri).await.unwrap())
}
/// Recorded answers by exact URL; anything else is a 404.
struct Recorded(std::collections::BTreeMap<String, (u16, String, Vec<u8>)>);
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Transport for Recorded {
    async fn get(
        &self,
        url: &url::Url,
        _: &[std::net::SocketAddr],
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        let (status, kind, body) = self.0.get(url.as_str()).cloned().unwrap_or((
            404,
            "text/html".into(),
            b"not found".to_vec(),
        ));
        Ok(pinakes_core::resolver::egress::Fetched {
            permanent_redirect: false,
            truncated: false,
            validators: Default::default(),
            url: url.clone(),
            content_type: kind,
            status,
            retry_after_secs: 60,
            location: None,
            body,
        })
    }
}
struct PublicDns;
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<std::net::SocketAddr>> {
        Ok(vec![std::net::SocketAddr::new(
            [93, 184, 216, 34].into(),
            port,
        )])
    }
}
/// An egress that never reaches the network: every host resolves to a fixed public address, so
/// the egress' own checks (policy, private addresses) run as they would, and every request fails.
/// For tests whose links are refused, or answered from the store, before anything is fetched.
pub fn offline_egress(
    cfg: pinakes_core::config::EgressConfig,
) -> pinakes_core::resolver::egress::Egress {
    pinakes_core::resolver::egress::Egress::with_backends(
        cfg,
        std::sync::Arc::new(PublicDns),
        std::sync::Arc::new(Offline),
    )
}
struct Offline;
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Transport for Offline {
    async fn get(
        &self,
        url: &url::Url,
        _: &[std::net::SocketAddr],
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        anyhow::bail!("no network in tests: {}", url.host_str().unwrap_or(""))
    }
}
