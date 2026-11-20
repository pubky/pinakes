//! Shared test helpers: a small OpenAPI 3.1 response check, an official source's card from its
//! recorded answers for a test that needs one source (and skips without it), and what several
//! test files use alike: a transport's answer (`fetched`), a resolver (`PublicDns`), `GET`
//! through the router (`get`, `get_from`), a saved card, a log buffer.
#![allow(dead_code)]
pub mod media;
pub mod spotify_api;
use serde_json::Value;
use std::collections::BTreeSet;

/// How often `source`'s configured credential was refused in this test process
/// (`pinakes_source_token_refusals_total{source}`).
pub fn token_refusals(source: &str) -> u64 {
    pinakes_core::sources::token_refusals()
        .into_iter()
        .find(|(s, _)| *s == source)
        .map_or(0, |(_, n)| n)
}

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
/// content type, file)`, the files in `tests/fixtures/sources/{name}/`; a `.gz` file is served
/// unpacked, as the service sent it), or `None` when this build has no source named `name`: a
/// test that needs one source returns early without it.
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
            let mut body = std::fs::read(format!("{dir}/{file}")).unwrap();
            if file.ends_with(".gz") {
                let mut out = Vec::new();
                std::io::Read::read_to_end(
                    &mut flate2::read::GzDecoder::new(body.as_slice()),
                    &mut out,
                )
                .unwrap();
                body = out;
            }
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
/// Recorded answers by exact URL; a request for anything else fails the test.
struct Recorded(std::collections::BTreeMap<String, (u16, String, Vec<u8>)>);
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Transport for Recorded {
    async fn get(
        &self,
        url: &url::Url,
        _: &[std::net::SocketAddr],
    ) -> anyhow::Result<pinakes_core::resolver::egress::Fetched> {
        let (status, kind, body) = self
            .0
            .get(url.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("unrecorded request {url}"));
        Ok(fetched(url.clone(), status, kind, body))
    }
}
/// Resolves every host to one public address (93.184.216.34), so the egress' own address check
/// passes and no test asks a real resolver. `media::PublicDns` answers 8.8.8.8 instead.
pub struct PublicDns;
#[async_trait::async_trait]
impl pinakes_core::resolver::egress::Dns for PublicDns {
    async fn lookup(&self, _: &str, port: u16) -> anyhow::Result<Vec<std::net::SocketAddr>> {
        Ok(vec![std::net::SocketAddr::new(
            [93, 184, 216, 34].into(),
            port,
        )])
    }
}
/// An answer as a test's transport gives it: `status` with `body` of `content_type` at `url`,
/// whole, with no redirect, no validators, no `Location` and a retry-after of 60 s. A test that
/// needs another value sets that field with struct update syntax.
pub fn fetched(
    url: url::Url,
    status: u16,
    content_type: String,
    body: Vec<u8>,
) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url,
        content_type,
        status,
        retry_after_secs: 60,
        location: None,
        body,
    }
}
/// `groups::drain`, then each grouping run it left deferred done again at once through
/// `groups::sync`, which has neither `groups::reconcile`'s 2 s statement timeout nor its retry
/// window. On a loaded machine a statement can outlast that timeout (`group_failed`, retried a
/// minute later), and a run can meet a busy writer (`lock_busy`, a few seconds later); either
/// way a single drain leaves the unit ungrouped. These tests are about what a run decides, not
/// when it runs. An error of the run done again fails the test, with its message.
pub async fn drain_groups(pool: &sqlx::PgPool, cfg: &pinakes_core::groups::GroupConfig) {
    pinakes_core::groups::drain(pool, cfg, 100).await.unwrap();
    let deferred: Vec<String> = sqlx::query_scalar(
        "SELECT resource_id FROM group_dirty WHERE last_error IS NOT NULL ORDER BY resource_id",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for id in deferred {
        let mut tx = pool.begin().await.unwrap();
        pinakes_core::groups::sync(&mut tx, &id, cfg)
            .await
            .unwrap_or_else(|e| panic!("grouping {id}: {e:#}"));
        tx.commit().await.unwrap();
    }
}
/// Every unit's group computed anew, as after a change of the grouping policy
/// (`groups::configure`): what grouping last read of each resource is forgotten, so no run is
/// skipped as unchanged, each resource with a unit is marked, and the runs are drained
/// (`drain_groups`). For a test of what grouping never reads: the run after the write under test
/// is a real one. Fails when a run is left waiting (more than a hundred resources, or one not due).
pub async fn regroup_all(pool: &sqlx::PgPool, cfg: &pinakes_core::groups::GroupConfig) {
    sqlx::query("DELETE FROM group_input")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_dirty(resource_id) SELECT DISTINCT resource_id FROM group_unit ON CONFLICT DO NOTHING")
        .execute(pool)
        .await
        .unwrap();
    drain_groups(pool, cfg).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM group_dirty")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "a group run is left waiting");
}
/// Whether the resources `ids`, of one unit each, share one group: the control of a test about
/// what grouped members do not do, or about what does not group.
pub async fn one_group(pool: &sqlx::PgPool, ids: &[&str]) -> bool {
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    let groups: Vec<Option<String>> =
        sqlx::query_scalar("SELECT group_id FROM group_unit WHERE resource_id=ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await
            .unwrap();
    groups.len() == ids.len() && groups[0].is_some() && groups.iter().all(|g| *g == groups[0])
}
/// This thread's CPU time, for a bound on the cost of a hostile input: it holds under any load on
/// the machine, where a wall-clock bound does not.
pub fn thread_cpu_time() -> std::time::Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes only the `timespec` it is given.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) };
    assert_eq!(
        rc, 0,
        "clock_gettime failed: a bound on it would hold nothing"
    );
    std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}
/// How many backends of this database wait for a lock right now.
pub async fn lock_waiters(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
/// Waits until at least `n` backends of this database wait for a lock: the sign that a spawned
/// writer has reached the row or key it must queue behind, in place of a sleep "long enough"
/// (short under load, long otherwise). Bounded: `what` names the writer that never got there.
pub async fn lock_waiters_reach(pool: &sqlx::PgPool, n: i64, what: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while lock_waiters(pool).await < n {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never waited for the lock"));
}
/// A pool of one connection to `pool`'s database: what a test runs through it runs on one
/// backend, whose statistics `counted` reads.
pub async fn one_connection(pool: &sqlx::PgPool) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}
/// A statistics counter (`query` selects one bigint), read through `connection`
/// (`one_connection`) once its backend's own counts are flushed: the work that backend did, in
/// place of a time. Other backends' counts arrive when they flush, so a bound on a database-wide
/// counter leaves them room.
pub async fn counted(connection: &sqlx::PgPool, query: &str) -> i64 {
    sqlx::query("SELECT pg_stat_force_next_flush()")
        .execute(connection)
        .await
        .unwrap();
    sqlx::query_scalar::<_, i64>(query)
        .fetch_one(connection)
        .await
        .unwrap()
}
/// An egress configuration for a mocked transport: no pacing between requests to one host, the
/// hosts with a published rate included (a `host_rates` entry wins over the published rate), so
/// recorded answers come back at once. The pacing itself is proven in `egress::tests` on a paused
/// clock.
pub fn unpaced_egress_config() -> pinakes_core::config::EgressConfig {
    let mut cfg = pinakes_core::config::EgressConfig {
        host_requests_per_second: 1000.0,
        ..Default::default()
    };
    for (host, _) in pinakes_core::resolver::egress::PUBLISHED_RATES {
        if !host.starts_with("*.") {
            cfg.host_rates.insert((*host).to_owned(), 1000.0);
        }
    }
    // The `*.wikipedia.org` rule: the languages the fixtures hold.
    for language in [
        "en", "de", "fr", "it", "pt", "ru", "zh", "fa", "es", "ja", "nl", "pl", "sv",
    ] {
        cfg.host_rates
            .insert(format!("{language}.wikipedia.org"), 1000.0);
    }
    cfg
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
/// Sends `GET path` to the router as the client at `peer` (`address:port`): the status and the
/// whole body of the answer.
async fn get_bytes(app: &axum::Router, peer: &str, path: &str) -> (u16, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}
/// `GET path` as the client at 127.0.0.2:1234, which must answer 200: the JSON body, checked
/// against the public document.
pub async fn get(app: &axum::Router, path: &str) -> Value {
    let (status, bytes) = get_bytes(app, "127.0.0.2:1234", path).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let value = serde_json::from_slice(&bytes).unwrap();
    check_response("GET", path, status, &value);
    value
}
/// `GET path` as the client at `peer` (`address:port`): the status and the JSON body, `Null`
/// when the body is not JSON. A 200 is checked against the public document.
pub async fn get_from(app: &axum::Router, peer: &str, path: &str) -> (u16, Value) {
    let (status, bytes) = get_bytes(app, peer, path).await;
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 200 {
        check_response("GET", path, status, &value);
    }
    (status, value)
}
/// Saves a card of type `kind` for `uri`, read as JSON-LD: the resource's id.
pub async fn save_card(pool: &sqlx::PgPool, uri: &str, kind: &str) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    let mut doc = pinakes_core::model::ResourceMeta::new(uri, "jsonld", 86400);
    doc.set_type(kind);
    pinakes_core::store::save_document(pool, &id, 0, &doc)
        .await
        .unwrap();
    id
}
/// The strings `sql` selects for `id`, its only parameter.
pub async fn strings(pool: &sqlx::PgPool, sql: &str, id: &str) -> Vec<String> {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_all(pool)
        .await
        .unwrap()
}
/// The error a resource's also-on retry is queued with, and in how many seconds it is due.
pub async fn also_on_due(pool: &sqlx::PgPool, id: &str) -> (Option<String>, f64) {
    sqlx::query_as("SELECT last_error, extract(epoch FROM next_attempt_at-now())::float8 FROM also_on_dirty WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// A recorded hub answer: the file `name` of `tests/fixtures/hubs`.
pub fn hubs_fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/hubs/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}
/// The `n`th author of a test: a public key in z-base-32, 52 characters.
pub fn author(n: usize) -> String {
    let alphabet = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    let mut v = vec![b'y'; 52];
    for i in 0..5 {
        v[50 - i] = alphabet[(n >> (5 * i)) & 31];
    }
    String::from_utf8(v).unwrap()
}
/// The IDs a card's `maybe_same` hint names, best first.
pub fn maybe_same_ids(card: &Value) -> Vec<&str> {
    card["maybe_same"]
        .as_array()
        .unwrap_or_else(|| panic!("no maybe_same: {card}"))
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect()
}
/// Log lines a test's subscriber writes.
#[derive(Clone, Default)]
pub struct Lines(pub std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
