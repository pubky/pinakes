//! `POST /mcp`: a Model Context Protocol endpoint (Streamable HTTP, stateless, read-only), with
//! the official Rust SDK, `rmcp`. Each tool is a request to the public routes, answered by their
//! own handlers under the public listener's layers (the per-IP budget, the size caps, the
//! statement timeout), with the caller's address: an MCP message costs one unit of the budget
//! and a tool call what its REST request costs on top. The admin routes are not in that router,
//! and no header of the MCP request reaches it, so the admin token is never read here. The
//! answer is compact JSON text: the card fields an agent needs and the REST URL of the full
//! answer. `social` asks the Nexus this instance reads for tags per URI, charged one unit.
use super::base_url;
use crate::api::AppState;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Method, Request},
    Router,
};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool, ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData, ServerHandler,
};
use serde_json::{json, Map, Value};
use std::{net::SocketAddr, sync::Arc};
use tower::ServiceExt;

/// The largest REST answer a tool reads (a search page of 500 cards fits).
const ANSWER_CAP: usize = 16 * 1024 * 1024;
/// The longest description a compact card keeps.
const DESCRIPTION_CHARS: usize = 300;

/// The MCP service: stateless, JSON answers, a body cap of the REST routes' 2 MB. Any `Host` is
/// accepted: the endpoint is public, read-only and takes no credentials, so the DNS-rebinding
/// guard rmcp applies to local servers protects nothing here, and the operator's host names are
/// the reverse proxy's to know.
pub fn service(rest: Router, state: AppState) -> StreamableHttpService<Tools, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(2 * 1024 * 1024)
        .disable_allowed_hosts();
    let tools = Tools { rest, state };
    StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

/// The tools, over the public routes (`rest`, without `/mcp`).
#[derive(Clone)]
pub struct Tools {
    rest: Router,
    state: AppState,
}

/// What a tool needs from the MCP request: the caller's address and the base URL.
struct Caller {
    peer: Option<ConnectInfo<SocketAddr>>,
    base: String,
}

/// A tool's failure, shown to the agent: the REST status and message, and what to do.
#[derive(Debug)]
struct Failure {
    status: u16,
    error: String,
    url: Option<String>,
}
impl Failure {
    fn bad(error: impl Into<String>) -> Self {
        Self {
            status: 400,
            error: error.into(),
            url: None,
        }
    }
    fn result(self) -> CallToolResult {
        let hint = match self.status {
            400 => "fix the argument the error names",
            404 => "unknown here: nobody referenced it on Pubky, or it has not been read; try `card` with resolve: true, or `search` by q or key",
            429 => "over the per-IP budget: wait a minute, then ask fewer, larger questions",
            502..=504 => "an upstream did not answer in time: retry once",
            _ => "retry once",
        };
        let mut body = json!({"error": self.error, "status": self.status, "hint": hint});
        if let Some(url) = self.url {
            body["url"] = url.into();
        }
        CallToolResult::error(vec![ContentBlock::text(body.to_string())])
    }
}

impl Tools {
    /// A request to the public routes as the MCP caller, and its JSON answer.
    async fn rest(
        &self,
        caller: &Caller,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
    ) -> Result<Value, Failure> {
        let url = format!("{}{path_and_query}", caller.base);
        let mut request = Request::builder().method(method).uri(path_and_query);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let mut request = request
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .map_err(|_| Failure::bad("the arguments do not make a request"))?;
        if let Some(peer) = caller.peer {
            request.extensions_mut().insert(peer);
        }
        let response = self
            .rest
            .clone()
            .oneshot(request)
            .await
            .unwrap_or_else(|never| match never {});
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), ANSWER_CAP)
            .await
            .map_err(|_| Failure {
                status: 500,
                error: "the answer was too large to read".into(),
                url: Some(url.clone()),
            })?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            json!({"error": String::from_utf8_lossy(&bytes).chars().take(300).collect::<String>()})
        });
        if !status.is_success() {
            return Err(Failure {
                status: status.as_u16(),
                error: value["error"]
                    .as_str()
                    .unwrap_or("request failed")
                    .to_owned(),
                url: Some(url),
            });
        }
        Ok(value)
    }

    async fn search(&self, caller: &Caller, args: &JsonObject) -> Result<Value, Failure> {
        let query = search_query(args)?;
        let path = format!("/v1/search?{query}");
        let page = self.rest(caller, Method::GET, &path, None).await?;
        Ok(page_answer(&caller.base, &path, &page))
    }

    async fn card(&self, caller: &Caller, args: &JsonObject) -> Result<Value, Failure> {
        let uri = string(args, "uri")?.ok_or_else(|| Failure::bad("`uri` is required"))?;
        let path = format!("/v1/resources/by-uri?{}", encode(&[("uri", uri.clone())]));
        let detail = match self.rest(caller, Method::GET, &path, None).await {
            Err(failure)
                if failure.status == 404 && args.get("resolve") == Some(&Value::Bool(true)) =>
            {
                self.rest(
                    caller,
                    Method::POST,
                    "/v1/resolve",
                    Some(json!({ "uri": uri })),
                )
                .await?
            }
            answer => answer?,
        };
        let mut card = compact(&caller.base, &detail);
        let details = &detail["details"];
        for key in [
            "category",
            "address",
            "facts",
            "tz",
            "event_status",
            "venue",
            "organizer",
        ] {
            if !details[key].is_null() {
                card[key] = details[key].clone();
            }
        }
        let aliases: Vec<Value> = details["aliases"]
            .as_array()
            .map(|a| a.iter().take(10).cloned().collect())
            .unwrap_or_default();
        if !aliases.is_empty() {
            card["aliases"] = aliases.into();
        }
        Ok(json!({"url": format!("{}{path}", caller.base), "card": card}))
    }

    async fn lookup(&self, caller: &Caller, args: &JsonObject) -> Result<Value, Failure> {
        let key = string(args, "key")?.ok_or_else(|| Failure::bad("`key` is required"))?;
        let mut pairs = vec![("key", key)];
        if let Some(limit) = number(args, "limit")? {
            pairs.push(("limit", limit));
        }
        let path = format!("/v1/search?{}", encode(&pairs));
        let page = self.rest(caller, Method::GET, &path, None).await?;
        Ok(page_answer(&caller.base, &path, &page))
    }

    async fn group(&self, caller: &Caller, args: &JsonObject) -> Result<Value, Failure> {
        let id = string(args, "id")?.ok_or_else(|| Failure::bad("`id` is required"))?;
        if !crate::model::valid_id(&id) {
            return Err(Failure::bad(
                "`id` is a 32-character group ID (a card's `group.id`)",
            ));
        }
        let path = format!("/v1/groups/{id}");
        let group = self.rest(caller, Method::GET, &path, None).await?;
        let members: Vec<Value> = group["members"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| compact(&caller.base, m))
            .collect();
        let best = best_member(group["type"].as_str(), &members);
        Ok(json!({
            "url": format!("{}{path}", caller.base),
            "id": group["id"],
            "type": group["type"],
            "keys": group["keys"],
            "best": best,
            "members": members,
        }))
    }

    async fn types(&self, caller: &Caller) -> Result<Value, Failure> {
        let path = "/v1/search?facet=type&limit=1";
        let page = self.rest(caller, Method::GET, path, None).await?;
        let info = self.rest(caller, Method::GET, "/v1/info", None).await?;
        Ok(json!({
            "url": format!("{}{path}", caller.base),
            "types": page["facets"]["type"],
            "resolves": info["resolves"],
        }))
    }

    async fn social(&self, caller: &Caller, args: &JsonObject) -> Result<Value, Failure> {
        let uris: Vec<String> = match args.get("uris") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()
                .ok_or_else(|| Failure::bad("`uris` is an array of strings"))?,
            _ => return Err(Failure::bad("`uris` is required: an array of 1–100 links")),
        };
        if !(1..=100).contains(&uris.len()) || uris.iter().any(|u| u.is_empty() || u.len() > 2048) {
            return Err(Failure::bad(
                "`uris` takes 1–100 links of at most 2048 bytes",
            ));
        }
        let viewer = string(args, "viewer")?;
        if viewer
            .as_deref()
            .is_some_and(|v| !crate::nexus::valid_author(v))
        {
            return Err(Failure::bad("`viewer` is a 52-character pubky public key"));
        }
        if let Some(peer) = caller.peer {
            self.state
                .charge(peer.0.ip(), 1)
                .await
                .map_err(|e| Failure {
                    status: e.0.as_u16(),
                    error: e.1,
                    url: None,
                })?;
        }
        let views = self
            .state
            .nexus
            .resources_by_uris(&uris, viewer.as_deref())
            .await
            .map_err(|_| Failure {
                status: 502,
                error: "the Nexus this instance reads did not answer the batch resource view (POST /v0/stream/resources/by_ids)".into(),
                url: None,
            })?;
        Ok(social_answer(&uris, &views))
    }
}

impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("pinakes", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parts = context.extensions.get::<axum::http::request::Parts>();
        let caller = Caller {
            peer: parts.and_then(|p| p.extensions.get::<ConnectInfo<SocketAddr>>().copied()),
            base: parts.map_or_else(|| "http://localhost".into(), |p| base_url(&p.headers)),
        };
        let args = request.arguments.unwrap_or_default();
        let answer = match request.name.as_ref() {
            "search" => self.search(&caller, &args).await,
            "card" => self.card(&caller, &args).await,
            "lookup" => self.lookup(&caller, &args).await,
            "group" => self.group(&caller, &args).await,
            "types" => self.types(&caller).await,
            "social" => self.social(&caller, &args).await,
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no tool named {other}"),
                    None,
                ))
            }
        };
        Ok(match answer {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(failure) => failure.result(),
        }
        .into())
    }
}

const INSTRUCTIONS: &str = "Pinakes indexes the external links people reference on Pubky as typed cards (schema.org types): places, events, books, music, papers, posts on other networks. Search it with `search`, read one link with `card`, find every link with one identifier (isbn:, doi:, isrc:, osm:, …) with `lookup`, list the same thing on several services with `group`, see what kinds of cards exist with `types`. Pinakes ids are its own: join with Nexus by URI. `social` returns tags and tagger counts per URI from Nexus. For the social graph (follows, trust, threads) use nexus-scout. Each answer names the REST URL with the full result. The guide is GET /llms.txt on this host.";

/// The tools and their input schemas.
pub fn tools() -> Vec<Tool> {
    let read_only = || {
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false)
    };
    let tool = |name: &'static str, description: &'static str, schema: Value| {
        let Value::Object(schema) = schema else {
            unreachable!("a tool's schema is an object")
        };
        Tool::new(name, description, Arc::new(schema)).with_annotations(read_only())
    };
    vec![
        tool(
            "search",
            "Search the links people referenced on Pubky. Every filter is optional and they combine: text, a schema.org type (Event includes MusicEvent; Place includes cafés), tags, a circle on the map, a time window, one exact identifier. Returns compact cards (id, uri, type, name, description, site, point, start, end, group, also_on) and a cursor for the next page.",
            json!({"type":"object","additionalProperties":false,"properties":{
                "q":{"type":"string","description":"Words in the name or description, at most 256 bytes."},
                "type":{"type":"array","items":{"type":"string"},"maxItems":10,"description":"Schema.org types, any of them, each with its subtypes: e.g. [\"Place\"], [\"Event\"], [\"MusicRecording\", \"MusicAlbum\", \"MusicGroup\"]. A single type as a string is accepted too."},
                "tags":{"type":"array","items":{"type":"string"},"maxItems":5,"description":"Labels people tagged the links with; any of them unless match is all."},
                "match":{"type":"string","enum":["any","all"],"description":"With tags: any (default) or all (at most 3 tags)."},
                "near":{"type":"object","required":["lat","lon","radius"],"additionalProperties":false,"properties":{"lat":{"type":"number"},"lon":{"type":"number"},"radius":{"type":"number","description":"Metres, at most 200000 by default."}},"description":"A circle on the map."},
                "bbox":{"type":"array","items":{"type":"number"},"minItems":4,"maxItems":4,"description":"A map viewport: west, south, east, north in degrees. Not with near."},
                "from":{"type":"string","description":"Earliest start: RFC 3339, or relative such as now, now+7d, now-12h."},
                "to":{"type":"string","description":"Latest start, same forms as from."},
                "key":{"type":"string","description":"One exact identifier: isbn:…, doi:…, isrc:…, upc:…, mbid:…, wikidata:Q…, imdb:tt…, podcast:…, osm:node/…"},
                "sort":{"type":"string","enum":["activity","recent","distance","start","trending"],"description":"activity (default), recent, distance (needs near), start (earliest first), trending (with window)."},
                "window":{"type":"string","description":"With sort trending: how far back, 1h to 90d, default 7d."},
                "collapse":{"type":"string","enum":["group"],"description":"group: one row per group of links that are the same thing."},
                "reach":{"type":"string","enum":["following","friends"],"description":"Only links that the viewer's follows (or friends) referenced. Needs viewer."},
                "viewer":{"type":"string","description":"With reach: a 52-character pubky public key."},
                "limit":{"type":"integer","minimum":1,"maximum":500,"description":"Cards per page, default 50."},
                "cursor":{"type":"string","description":"The cursor of the previous page, with the same filters."}
            }}),
        ),
        tool(
            "card",
            "Read one link by its URI: its card with details (address, facts from official APIs, other spellings, the same thing on other services). Only links this instance already knows; set resolve to true to have an unknown link read now (it has its own small per-minute budget and may answer pending).",
            json!({"type":"object","required":["uri"],"additionalProperties":false,"properties":{
                "uri":{"type":"string","description":"The link, in any spelling."},
                "resolve":{"type":"boolean","description":"Read the link now if it is unknown here. Default false."}
            }}),
        ),
        tool(
            "lookup",
            "Every link carrying one exact identifier, on any service and in any spelling: a book's ISBN, a paper's DOI, a recording's ISRC, an album's UPC, a MusicBrainz id, a Wikidata item, an IMDb title, a podcast GUID, an OpenStreetMap object.",
            json!({"type":"object","required":["key"],"additionalProperties":false,"properties":{
                "key":{"type":"string","description":"isbn:9780140328721, doi:10.1038/nphys1170, isrc:GBAYE9701376, upc:…, mbid:…, wikidata:Q42, imdb:tt0111161, podcast:…, osm:node/240109189"},
                "limit":{"type":"integer","minimum":1,"maximum":500}
            }}),
        ),
        tool(
            "group",
            "The links that are the same thing on several services (a song on Spotify and Apple Music, a book's editions, one event on two platforms), by the group id a card names. Returns every member's card and the best member: the most complete card of the group's type.",
            json!({"type":"object","required":["id"],"additionalProperties":false,"properties":{
                "id":{"type":"string","description":"A card's group.id: 32 lowercase hex characters."}
            }}),
        ),
        tool(
            "types",
            "What this instance holds: the schema.org card types with how many links of each, and the kinds of links it resolves.",
            json!({"type":"object","additionalProperties":false,"properties":{}}),
        ),
        tool(
            "social",
            "Tags and tagger counts per link, from the Nexus this instance reads (its batch resource view). Give the uris of cards from search; with viewer, each tag says whether that user used it. Links Nexus does not know are listed under unknown.",
            json!({"type":"object","required":["uris"],"additionalProperties":false,"properties":{
                "uris":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":100},
                "viewer":{"type":"string","description":"A 52-character pubky public key."}
            }}),
        ),
    ]
}

/// A string argument, if given.
fn string(args: &JsonObject, key: &str) -> Result<Option<String>, Failure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(Failure::bad(format!("`{key}` is a string"))),
    }
}
/// A number argument as its decimal text, if given.
fn number(args: &JsonObject, key: &str) -> Result<Option<String>, Failure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(Failure::bad(format!("`{key}` is a number"))),
    }
}
fn encode(pairs: &[(&str, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// The `/v1/search` query of a `search` call: its arguments as the route's parameters.
fn search_query(args: &JsonObject) -> Result<String, Failure> {
    const TEXT: &[(&str, &str)] = &[
        ("q", "q"),
        ("match", "match"),
        ("from", "from"),
        ("to", "to"),
        ("key", "key"),
        ("sort", "sort"),
        ("window", "window"),
        ("collapse", "collapse"),
        ("reach", "reach"),
        ("viewer", "viewer"),
        ("cursor", "cursor"),
    ];
    let known = [
        "q", "type", "tags", "match", "near", "bbox", "from", "to", "key", "sort", "window",
        "collapse", "reach", "viewer", "limit", "cursor",
    ];
    if let Some(unknown) = args.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(Failure::bad(format!(
            "`{unknown}` is not an argument of search"
        )));
    }
    let mut pairs: Vec<(&str, String)> = vec![];
    for (arg, param) in TEXT {
        if let Some(value) = string(args, arg)? {
            pairs.push((param, value));
        }
    }
    // `type` and `tags`: an array, or the route's comma-separated string.
    for key in ["type", "tags"] {
        match args.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                let items: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
                let items =
                    items.ok_or_else(|| Failure::bad(format!("`{key}` is an array of strings")))?;
                if !items.is_empty() {
                    pairs.push((key, items.join(",")));
                }
            }
            Some(Value::String(value)) => pairs.push((key, value.clone())),
            Some(_) => return Err(Failure::bad(format!("`{key}` is an array of strings"))),
        }
    }
    match args.get("near") {
        None | Some(Value::Null) => {}
        Some(Value::Object(near)) => {
            let coordinate = |key: &str| {
                near.get(key)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| Failure::bad(format!("`near.{key}` is a number")))
            };
            let (lat, lon, radius) = (
                coordinate("lat")?,
                coordinate("lon")?,
                coordinate("radius")?,
            );
            pairs.push(("near", format!("{lat},{lon}")));
            pairs.push(("radius", radius.to_string()));
        }
        Some(_) => return Err(Failure::bad("`near` is {lat, lon, radius}")),
    }
    match args.get("bbox") {
        None | Some(Value::Null) => {}
        Some(Value::Array(edges)) => {
            let edges: Option<Vec<String>> = edges
                .iter()
                .map(|e| e.as_f64().map(|e| e.to_string()))
                .collect();
            match edges {
                Some(edges) if edges.len() == 4 => pairs.push(("bbox", edges.join(","))),
                _ => return Err(Failure::bad("`bbox` is [west, south, east, north]")),
            }
        }
        Some(_) => return Err(Failure::bad("`bbox` is [west, south, east, north]")),
    }
    if let Some(limit) = number(args, "limit")? {
        pairs.push(("limit", limit));
    }
    Ok(encode(&pairs))
}

/// A card with the fields an agent needs, and the URL of its full read.
fn compact(base: &str, card: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id", "uri", "status", "type", "name", "site", "point", "start", "end", "image",
    ] {
        if !card[key].is_null() {
            out.insert(key.into(), card[key].clone());
        }
    }
    if let Some(description) = card["description"].as_str() {
        let mut short: String = description.chars().take(DESCRIPTION_CHARS).collect();
        if short.len() < description.len() {
            short.push('…');
        }
        out.insert("description".into(), short.into());
    }
    if card["group"].is_object() {
        out.insert("group".into(), card["group"].clone());
    }
    let also_on: Vec<Value> = card["also_on"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| json!({"uri": a["uri"], "site": a["site"]}))
        .collect();
    if !also_on.is_empty() {
        out.insert("also_on".into(), also_on.into());
    }
    for key in ["occurrence", "event", "attribution"] {
        if !card[key].is_null() {
            out.insert(key.into(), card[key].clone());
        }
    }
    if let Some(id) = card["id"].as_str() {
        out.insert("url".into(), format!("{base}/v1/resources/{id}").into());
    }
    Value::Object(out)
}

/// A search page as a tool answers it.
fn page_answer(base: &str, path: &str, page: &Value) -> Value {
    let items: Vec<Value> = page["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|card| compact(base, card))
        .collect();
    let mut answer =
        json!({"url": format!("{base}{path}"), "items": items, "cursor": page["cursor"]});
    if page["reach_truncated"] == true {
        answer["reach_truncated"] = true.into();
    }
    if page["facets"].is_object() {
        answer["facets"] = page["facets"].clone();
    }
    answer
}

/// The most complete member of the group's type: a readable card with a name, then the most of
/// image, description and place or time; ties by member order.
fn best_member(group_type: Option<&str>, members: &[Value]) -> Value {
    members
        .iter()
        .enumerate()
        .max_by_key(|(i, m)| {
            let ok = m["status"] == "ok";
            let typed = group_type.is_some_and(|t| m["type"] == t);
            let fields = ["name", "image", "description", "point", "start"]
                .iter()
                .filter(|k| !m[**k].is_null())
                .count();
            (ok, typed, fields, std::cmp::Reverse(*i))
        })
        .map_or(Value::Null, |(_, m)| m.clone())
}

/// What `social` answers: per URI asked, the tags and tagger count Nexus gave, and the URIs
/// it did not know.
fn social_answer(uris: &[String], views: &[Value]) -> Value {
    let mut items = vec![];
    let mut unknown = vec![];
    for uri in uris {
        let normalized = crate::model::identity(uri).map(|(_, u, _)| u).ok();
        let view = views.iter().find(|v| {
            v["details"]["uri"].as_str().is_some_and(|u| {
                u == uri
                    || normalized.as_deref() == Some(u)
                    || crate::model::identity(u).is_ok_and(|(_, n, _)| Some(n) == normalized)
            })
        });
        let Some(view) = view else {
            unknown.push(Value::from(uri.as_str()));
            continue;
        };
        let tags: Vec<Value> = view["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let mut tag = json!({"label": t["label"], "taggers_count": t["taggers_count"]});
                if t["relationship"].is_boolean() {
                    tag["viewer_tagged"] = t["relationship"].clone();
                }
                tag
            })
            .collect();
        items.push(json!({"uri": uri, "taggers_count": view["taggers_count"], "tags": tags}));
    }
    json!({"items": items, "unknown": unknown})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> JsonObject {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn search_arguments_become_the_routes_parameters() {
        let query = search_query(&args(json!({
            "q": "café", "type": "Place", "tags": ["coffee", "wifi"], "match": "all",
            "near": {"lat": 47.3769, "lon": 8.5417, "radius": 2000}, "from": "now", "to": "now+7d",
            "sort": "distance", "limit": 10
        })))
        .unwrap();
        assert_eq!(
            query,
            "q=caf%C3%A9&match=all&from=now&to=now%2B7d&sort=distance&type=Place&tags=coffee%2Cwifi&near=47.3769%2C8.5417&radius=2000&limit=10"
        );
        assert_eq!(
            search_query(&args(json!({"bbox": [8.45, 47.33, 8.62, 47.42]}))).unwrap(),
            "bbox=8.45%2C47.33%2C8.62%2C47.42"
        );
        assert_eq!(
            search_query(&args(json!({"type": ["MusicRecording", "MusicAlbum"]}))).unwrap(),
            "type=MusicRecording%2CMusicAlbum"
        );
        for bad in [
            json!({"admin": true}),
            json!({"q": 1}),
            json!({"tags": [1]}),
            json!({"type": [1]}),
            json!({"near": {"lat": 1}}),
            json!({"bbox": [1, 2, 3]}),
            json!({"limit": "ten"}),
        ] {
            assert!(search_query(&args(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_compact_card_keeps_what_an_agent_needs() {
        let card = json!({
            "id": "a".repeat(32), "uri": "https://example.org/", "status": "ok", "type": "WebPage",
            "name": "Example", "description": "x".repeat(400), "image": null, "site": "example.org",
            "point": null, "start": null, "end": null, "group": null, "open_in": [],
            "also_on": [{"id": "b", "uri": "https://example.com/", "site": "Example", "derived": false}]
        });
        let compact = compact("https://pinakes.example", &card);
        assert_eq!(
            compact["url"],
            format!("https://pinakes.example/v1/resources/{}", "a".repeat(32))
        );
        assert_eq!(
            compact["description"].as_str().unwrap().chars().count(),
            301
        );
        assert_eq!(
            compact["also_on"],
            json!([{"uri": "https://example.com/", "site": "Example"}])
        );
        for absent in ["image", "point", "group", "open_in"] {
            assert!(compact.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn the_best_member_is_the_most_complete_card_of_the_groups_type() {
        let members = vec![
            json!({"id": "1", "status": "ok", "type": "MusicRecording", "name": "Song"}),
            json!({"id": "2", "status": "ok", "type": "MusicRecording", "name": "Song", "image": "i", "description": "d"}),
            json!({"id": "3", "status": "missing", "type": "MusicRecording", "name": "Song", "image": "i", "description": "d", "start": "s"}),
            json!({"id": "4", "status": "ok", "type": "WebPage", "name": "Song", "image": "i", "description": "d", "start": "s"}),
        ];
        assert_eq!(best_member(Some("MusicRecording"), &members)["id"], "2");
        assert_eq!(best_member(None, &members)["id"], "4");
        assert_eq!(best_member(None, &[]), Value::Null);
    }

    #[test]
    fn social_matches_views_by_uri_in_any_spelling() {
        let uris = vec![
            "https://WWW.Example.org/a".to_owned(),
            "https://example.org/unknown".to_owned(),
        ];
        let views = vec![json!({
            "details": {"id": "x", "uri": "https://www.example.org/a"},
            "taggers_count": 2,
            "tags": [{"label": "cafe", "taggers_count": 2, "relationship": true, "taggers": ["k"]}]
        })];
        assert_eq!(
            social_answer(&uris, &views),
            json!({
                "items": [{"uri": "https://WWW.Example.org/a", "taggers_count": 2,
                           "tags": [{"label": "cafe", "taggers_count": 2, "viewer_tagged": true}]}],
                "unknown": ["https://example.org/unknown"]
            })
        );
    }

    #[test]
    fn every_tool_is_read_only_and_its_schema_an_object() {
        let tools = tools();
        assert_eq!(
            tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>(),
            ["search", "card", "lookup", "group", "types", "social"]
        );
        for tool in tools {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert_eq!(
                tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                Some(true)
            );
        }
    }

    #[test]
    fn a_failure_says_what_to_do() {
        let result = Failure {
            status: 429,
            error: "API rate limit exceeded".into(),
            url: Some("https://pinakes.example/v1/search?q=x".into()),
        }
        .result();
        assert_eq!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("text content")
        };
        let body: Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(body["status"], 429);
        assert!(body["hint"].as_str().unwrap().contains("wait a minute"));
    }
}
