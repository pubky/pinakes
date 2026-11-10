//! `POST /mcp`: a Model Context Protocol endpoint (Streamable HTTP, stateless, read-only), with
//! the official Rust SDK, `rmcp`. Each tool is a request to the public routes, answered by their
//! own handlers under the public listener's layers (the per-IP budget, the size caps, the
//! statement timeout), with the caller's address: an MCP message costs one unit of the budget
//! and a tool call what its REST request costs on top. The admin routes are not in that router,
//! and no header of the MCP request reaches it, so the admin token is never read here. The
//! answer is compact JSON text: the card fields an agent needs and the REST URL of the full
//! answer. `social` asks the Nexus this instance reads for tags per card, by the card's `uri`
//! and `refs`, charged one unit per 100 URIs.
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
        // A read is one flat object: the card's fields and the read's beside them.
        let mut card = compact(&caller.base, &detail);
        for key in [
            "category",
            "address",
            "facts",
            "keys",
            "tz",
            "event_status",
            "venue",
            "organizer",
        ] {
            if !detail[key].is_null() {
                card[key] = detail[key].clone();
            }
        }
        let aliases: Vec<Value> = detail["aliases"]
            .as_array()
            .map(|a| a.iter().take(10).cloned().collect())
            .unwrap_or_default();
        if !aliases.is_empty() {
            card["aliases"] = aliases.into();
        }
        // The public collections that hold it, newest first: the first few, and where the rest are.
        if let (Some(id), true) = (
            detail["id"].as_str(),
            detail["collections"].as_i64().is_some_and(|n| n > 0),
        ) {
            let list = format!("/v1/resources/{id}/collections?limit={IN_COLLECTIONS}");
            let page = self.rest(caller, Method::GET, &list, None).await?;
            card["in_collections"] = page["items"].clone();
            card["collections_url"] =
                format!("{}/v1/resources/{id}/collections", caller.base).into();
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
        let list = |key: &str| -> Result<Vec<String>, Failure> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(vec![]),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or_else(|| Failure::bad(format!("`{key}` is an array of strings"))),
                Some(_) => Err(Failure::bad(format!("`{key}` is an array of strings"))),
            }
        };
        let (ids, uris) = (list("ids")?, list("uris")?);
        if !(1..=100).contains(&(ids.len() + uris.len())) {
            return Err(Failure::bad(
                "`ids` (card ids) and `uris` (links) take 1–100 entries together",
            ));
        }
        if ids.iter().any(|id| !crate::model::valid_id(id)) {
            return Err(Failure::bad(
                "`ids` are card ids: 32 lowercase hex characters",
            ));
        }
        if uris.iter().any(|u| u.is_empty() || u.len() > 2048) {
            return Err(Failure::bad("`uris` are links of at most 2048 bytes"));
        }
        // A link that carries a credential is never sent to the Nexus: a call naming one is
        // refused, and a card's own link or ref that is one (a link someone's post made, read by
        // its URI) is left out of what is asked below.
        let credential =
            |u: &str| crate::capability::is_capability(u) || crate::capability::names_credential(u);
        if uris.iter().any(|u| credential(u)) {
            return Err(Failure::bad(
                "`uris` holds a link that carries a credential; it is never sent on",
            ));
        }
        let viewer = string(args, "viewer")?;
        if viewer
            .as_deref()
            .is_some_and(|v| !crate::nexus::valid_author(v))
        {
            return Err(Failure::bad("`viewer` is a 52-character pubky public key"));
        }
        // Each card is asked about by its own link and every link people referenced for it.
        let mut asks: Vec<Ask> = vec![];
        let mut unknown_ids = vec![];
        if !ids.is_empty() {
            let cards = self
                .rest(
                    caller,
                    Method::POST,
                    "/v1/resources/lookup",
                    Some(json!({ "ids": ids })),
                )
                .await?;
            // Lookup answers an alias (a spelling, or a card that became one after it was
            // read) with its root's card: match each id asked through its root.
            let roots: Vec<(String, String)> = sqlx::query_as(
                "SELECT wanted.id,coalesce(a.root_id,wanted.id) FROM unnest($1::text[]) AS wanted(id) LEFT JOIN resource_alias a ON a.alias_id=wanted.id",
            )
            .bind(&ids)
            .fetch_all(&self.state.pool)
            .await
            .map_err(|_| Failure {
                status: 500,
                error: "the card ids could not be read".into(),
                url: None,
            })?;
            for id in &ids {
                let root = roots
                    .iter()
                    .find(|(asked, _)| asked == id)
                    .map_or(id.as_str(), |(_, root)| root.as_str());
                let Some(card) = cards
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|c| c["id"] == root)
                else {
                    unknown_ids.push(id.clone());
                    continue;
                };
                let mut asked = vec![card["uri"].as_str().unwrap_or_default().to_owned()];
                for r in card["refs"].as_array().into_iter().flatten() {
                    if let Some(r) = r.as_str() {
                        if !asked.iter().any(|a| a == r) {
                            asked.push(r.to_owned());
                        }
                    }
                }
                asked.retain(|u| !credential(u));
                if asked.is_empty() {
                    continue;
                }
                asks.push(Ask {
                    id: Some(id.clone()),
                    root: (root != id).then(|| root.to_owned()),
                    uri: asked[0].clone(),
                    asked,
                });
            }
        }
        for uri in uris {
            asks.push(Ask {
                id: None,
                root: None,
                uri: uri.clone(),
                asked: vec![uri],
            });
        }
        let all: Vec<String> = asks.iter().flat_map(|a| a.asked.iter().cloned()).collect();
        let mut views = vec![];
        for batch in all.chunks(100) {
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
            views.extend(
                self.state
                    .nexus
                    .resources_by_uris(batch, viewer.as_deref())
                    .await
                    .map_err(|_| Failure {
                        status: 502,
                        error: "the Nexus this instance reads did not answer the batch resource view (POST /v0/stream/resources/by_ids)".into(),
                        url: None,
                    })?,
            );
        }
        let mut answer = social_answer(&asks, &views);
        if !unknown_ids.is_empty() {
            answer["unknown_ids"] = unknown_ids.into();
        }
        Ok(answer)
    }
}

/// What `social` asks Nexus about one card (or one link): the URIs, the card's own first. `id`
/// is the card id asked, `root` the card answering for it when it is an alias.
struct Ask {
    id: Option<String>,
    root: Option<String>,
    uri: String,
    asked: Vec<String>,
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

const INSTRUCTIONS: &str = "Pinakes indexes the external links people reference on Pubky as typed cards (schema.org types): places, events, books, music, papers, posts on other networks. Search it with `search`, read one link with `card`, find every link with one identifier (isbn:, doi:, isrc:, osm:, …) with `lookup`, list the same thing on several services with `group`, see what kinds of cards exist with `types`. Pinakes ids are its own: join with Nexus by URI, the card's `uri` and its `refs` (the links people wrote for it; a typed card's `uri`, such as btc: or isbn:, is often not one, and an occurrence's refs are its feed's, `occurrence.feed_uri`, not its event page). A full read's `aliases` lists every other spelling of the link. `social` does that join: tags and tagger counts per card, merged over its links. For the social graph (follows, trust, threads) use nexus-scout. Each answer names the REST URL with the full result. The guide is GET /llms.txt on this host.";

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
                "remote":{"type":"boolean","description":"true: job postings that may be done remotely; false: job postings that may not."},
                "closed":{"type":"boolean","description":"true: list job postings past their closing date too (left out by default)."},
                "ingredient":{"type":"string","description":"Recipes with these ingredients: up to 3, comma-separated, each matching whole words of an ingredient's name (flour matches all-purpose flour)."},
                "in_collection":{"type":"string","description":"The links one public collection holds: the collection post's pubky:// URI (from a card tool answer's in_collections)."},
                "sort":{"type":"string","enum":["activity","recent","distance","start","trending"],"description":"activity (default), recent, distance (needs near), start (earliest first), trending (with window)."},
                "window":{"type":"string","description":"With sort trending: how far back, 1h to 90d, default 7d."},
                "collapse":{"type":"string","enum":["group"],"description":"group: one row per group of links that are the same thing."},
                "reach":{"type":"string","enum":["following","friends"],"description":"Only links that the viewer's follows (or friends) referenced. Needs viewer."},
                "boost":{"type":"string","enum":["following","friends","wot_2"],"description":"Rank first the links the viewer's follows (or friends, or two steps of follows) referenced, leaving nothing out. Needs viewer; with the default sort or trending. wot_2 only where the instance has it."},
                "viewer":{"type":"string","description":"With reach or boost: a 52-character pubky public key."},
                "limit":{"type":"integer","minimum":1,"maximum":500,"description":"Cards per page, default 50."},
                "cursor":{"type":"string","description":"The cursor of the previous page, with the same filters."}
            }}),
        ),
        tool(
            "card",
            "Read one link by its URI: its card with details (address, facts from official APIs, other spellings, the same thing on other services, the public Pubky collections that hold it). Only links this instance already knows; set resolve to true to have an unknown link read now (it has its own small per-minute budget and may answer pending).",
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
            "Tags and taggers per card, from the Nexus this instance reads. Give the ids of cards from search: each card is asked about by its own uri and by every link people referenced for it (its refs, such as the mempool.space page of a btc: block, or an occurrence's feed, occurrence.feed_uri), and the answers are merged per card. Links that are not Pinakes cards, or a card's other spellings (aliases on its full read), can be given as uris. Taggers are counted once each; a count marked at_least is a lower bound (Nexus counts taggers it does not list, who may be the same on several links). With viewer, each tag says whether that user used it.",
            json!({"type":"object","additionalProperties":false,"properties":{
                "ids":{"type":"array","items":{"type":"string"},"maxItems":100,"description":"Card ids from search, card or lookup."},
                "uris":{"type":"array","items":{"type":"string"},"maxItems":100,"description":"Links to ask about as they are."},
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
        ("boost", "boost"),
        ("viewer", "viewer"),
        ("cursor", "cursor"),
        ("ingredient", "ingredient"),
        ("in_collection", "in_collection"),
    ];
    let known = [
        "q",
        "type",
        "tags",
        "match",
        "near",
        "bbox",
        "from",
        "to",
        "key",
        "sort",
        "window",
        "collapse",
        "reach",
        "boost",
        "viewer",
        "limit",
        "cursor",
        "remote",
        "closed",
        "ingredient",
        "in_collection",
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
    for flag in ["remote", "closed"] {
        match args.get(flag) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => pairs.push((flag, b.to_string())),
            Some(_) => return Err(Failure::bad(format!("`{flag}` is a boolean"))),
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
/// How many of a card's collections the `card` tool lists.
const IN_COLLECTIONS: usize = 5;
fn compact(base: &str, card: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id", "uri", "status", "type", "name", "site", "point", "start", "end", "image",
    ] {
        if !card[key].is_null() {
            out.insert(key.into(), card[key].clone());
        }
    }
    let mut shortened = false;
    if let Some(description) = card["description"].as_str() {
        let mut short: String = description.chars().take(DESCRIPTION_CHARS).collect();
        if short.len() < description.len() {
            short.push('…');
            shortened = true;
        }
        out.insert("description".into(), short.into());
    }
    // The credits of the image and description shown; a shortened description is an excerpt.
    let credits: Vec<Value> = card["credits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let mut c = c.clone();
            if shortened && c["field"] == "description" {
                c["excerpted"] = true.into();
            }
            c
        })
        .collect();
    out.insert("credits".into(), credits.into());
    if card["group"].is_object() {
        out.insert("group".into(), card["group"].clone());
    }
    // One link per service, at most eight, as a search card has them: a read lists every link.
    let mut sites = std::collections::HashSet::new();
    let also_on: Vec<Value> = card["also_on"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| sites.insert(a["site"].to_string()))
        .take(8)
        .map(|a| json!({"uri": a["uri"], "site": a["site"]}))
        .collect();
    if !also_on.is_empty() {
        out.insert("also_on".into(), also_on.into());
    }
    if card["refs"].as_array().is_some_and(|r| !r.is_empty()) {
        out.insert("refs".into(), card["refs"].clone());
    }
    if card["collections"].as_i64().is_some_and(|n| n > 0) {
        out.insert("collections".into(), card["collections"].clone());
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

/// Whether a Nexus view is the one for `uri`, in any spelling the two normalize alike.
fn view_of<'a>(views: &'a [Value], uri: &str) -> Option<&'a Value> {
    let normalized = crate::model::identity(uri).map(|(_, u, _)| u).ok();
    views.iter().find(|v| {
        v["details"]["uri"].as_str().is_some_and(|u| {
            u == uri
                || normalized.as_deref() == Some(u)
                || crate::model::identity(u).is_ok_and(|(_, n, _)| Some(n) == normalized)
        })
    })
}

/// A count of taggers over several views: the taggers they list, once each, or the most one
/// view counts, whichever is more. A view counts taggers it does not list (Nexus lists a bounded
/// number), and those may be the ones another view lists, so over several such views the count
/// is a lower bound (`at_least`), never a sum that counts one person twice.
#[derive(Default)]
struct Taggers {
    listed: std::collections::BTreeSet<String>,
    most: u64,
    views: usize,
    unlisted: bool,
}
impl Taggers {
    fn add<'a>(&mut self, listed: impl IntoIterator<Item = &'a Value>, count: &Value) {
        let names: std::collections::BTreeSet<&str> =
            listed.into_iter().filter_map(Value::as_str).collect();
        let count = count.as_u64().unwrap_or_default().max(names.len() as u64);
        self.unlisted |= count > names.len() as u64;
        self.most = self.most.max(count);
        self.views += 1;
        self.listed.extend(names.into_iter().map(str::to_owned));
    }
    fn count(&self) -> u64 {
        (self.listed.len() as u64).max(self.most)
    }
    fn at_least(&self) -> bool {
        self.unlisted && self.views > 1
    }
}
/// A tag line's listed taggers.
fn taggers(tag: &Value) -> impl Iterator<Item = &Value> {
    tag["taggers"].as_array().into_iter().flatten()
}

/// What `social` answers: per card (or link), the URIs asked and those Nexus knew, and the tags
/// of all of them merged: per label, its taggers once each; `viewer_tagged` when the viewer used
/// the label on any of them.
fn social_answer(asks: &[Ask], views: &[Value]) -> Value {
    let mut items = vec![];
    for ask in asks {
        let found: Vec<(&String, &Value)> = ask
            .asked
            .iter()
            .filter_map(|u| view_of(views, u).map(|v| (u, v)))
            .collect();
        let mut labels: Vec<(String, Taggers, bool)> = vec![];
        let mut everyone = Taggers::default();
        for (_, view) in &found {
            // A view counts its taggers over all labels (`taggers_count`), each once.
            let tags = view["tags"].as_array().into_iter().flatten();
            everyone.add(tags.clone().flat_map(taggers), &view["taggers_count"]);
            for tag in tags {
                let Some(label) = tag["label"].as_str() else {
                    continue;
                };
                let at = match labels.iter().position(|(l, _, _)| l == label) {
                    Some(at) => at,
                    None => {
                        labels.push((label.to_owned(), Taggers::default(), false));
                        labels.len() - 1
                    }
                };
                labels[at].1.add(taggers(tag), &tag["taggers_count"]);
                labels[at].2 |= tag["relationship"] == true;
            }
        }
        let mut tags: Vec<(String, Taggers, bool)> = labels;
        tags.sort_by(|a, b| b.1.count().cmp(&a.1.count()).then_with(|| a.0.cmp(&b.0)));
        let mut item = json!({
            "uri": ask.uri,
            "asked": ask.asked,
            "found": found.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
            "taggers_count": everyone.count(),
            "tags": tags.into_iter().map(|(label, taggers, mine)| {
                let mut tag = json!({"label": label, "taggers_count": taggers.count(), "viewer_tagged": mine});
                if taggers.at_least() {
                    tag["at_least"] = true.into();
                }
                tag
            }).collect::<Vec<_>>(),
        });
        if everyone.at_least() {
            item["at_least"] = true.into();
        }
        if let Some(id) = &ask.id {
            item["id"] = id.clone().into();
        }
        if let Some(root) = &ask.root {
            item["root"] = root.clone().into();
        }
        items.push(item);
    }
    json!({ "items": items })
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
        assert_eq!(
            search_query(&args(
                json!({"type": "JobPosting", "remote": true, "closed": false})
            ))
            .unwrap(),
            "type=JobPosting&remote=true&closed=false"
        );
        for bad in [
            json!({"remote": "yes"}),
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
        // Always there, as on every card.
        assert_eq!(compact["credits"], json!([]));
        // The credits of what it shows come along; a shortened description is an excerpt.
        let credit = |field: &str| {
            json!({"field": field, "provider": "wikipedia", "source_url": "https://en.wikipedia.org/wiki/X",
                   "author": "Wikipedia contributors", "license": "CC-BY-SA-4.0",
                   "license_url": "https://creativecommons.org/licenses/by-sa/4.0/", "lang": "en",
                   "excerpted": false, "modified": false, "subject": null})
        };
        let mut credited = card.clone();
        credited["credits"] = json!([credit("description")]);
        let compact = super::compact("https://pinakes.example", &credited);
        assert_eq!(compact["credits"][0]["excerpted"], true);
        assert_eq!(
            compact["credits"][0]["source_url"],
            "https://en.wikipedia.org/wiki/X"
        );
        credited["description"] = "short".into();
        let compact = super::compact("https://pinakes.example", &credited);
        assert_eq!(compact["credits"], json!([credit("description")]));
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
    fn social_merges_a_cards_links_and_counts_each_tagger_once() {
        let asks = vec![
            // A btc: card asked by its own link and the mempool page people tagged.
            Ask {
                id: Some("c1".into()),
                root: None,
                uri: "btc:block/0000aa".into(),
                asked: vec![
                    "btc:block/0000aa".into(),
                    "https://WWW.Mempool.space/block/0000aa".into(),
                    "https://blockstream.info/block/0000aa".into(),
                ],
            },
            Ask {
                id: None,
                root: None,
                uri: "https://example.org/unknown".into(),
                asked: vec!["https://example.org/unknown".into()],
            },
        ];
        let views = vec![
            json!({"details": {"uri": "https://www.mempool.space/block/0000aa"}, "taggers_count": 2,
                   "tags": [{"label": "halving", "taggers_count": 2, "relationship": true, "taggers": ["t1", "t2"]},
                            {"label": "bitcoin", "taggers_count": 1, "taggers": ["t1"]}]}),
            // The same tagger on another link counts once. One counted but not listed may be one
            // the other link lists: the count is a lower bound, never a sum.
            json!({"details": {"uri": "https://blockstream.info/block/0000aa"}, "taggers_count": 2,
                   "tags": [{"label": "halving", "taggers_count": 2, "taggers": ["t2"]}]}),
        ];
        assert_eq!(
            social_answer(&asks, &views),
            json!({"items": [
                {"id": "c1", "uri": "btc:block/0000aa",
                 "asked": ["btc:block/0000aa", "https://WWW.Mempool.space/block/0000aa", "https://blockstream.info/block/0000aa"],
                 "found": ["https://WWW.Mempool.space/block/0000aa", "https://blockstream.info/block/0000aa"],
                 "taggers_count": 2, "at_least": true,
                 "tags": [{"label": "halving", "taggers_count": 2, "viewer_tagged": true, "at_least": true},
                          {"label": "bitcoin", "taggers_count": 1, "viewer_tagged": false}]},
                {"uri": "https://example.org/unknown", "asked": ["https://example.org/unknown"],
                 "found": [], "taggers_count": 0, "tags": []}
            ]})
        );
    }

    /// The same two people tag two of a card's links; each link's view lists one of them and
    /// counts both. They are two at least, never four; one view alone is exact.
    #[test]
    fn unlisted_taggers_are_never_summed_over_views() {
        let ask = |asked: &[&str]| Ask {
            id: Some("c1".into()),
            root: None,
            uri: asked[0].into(),
            asked: asked.iter().map(|u| u.to_string()).collect(),
        };
        let view = |uri: &str, listed: &str| {
            json!({"details": {"uri": uri}, "taggers_count": 2,
                   "tags": [{"label": "halving", "taggers_count": 2, "taggers": [listed]}]})
        };
        let views = [
            view("https://mempool.space/block/0000aa", "t1"),
            view("https://blockstream.info/block/0000aa", "t2"),
        ];
        let both = social_answer(
            &[ask(&[
                "https://mempool.space/block/0000aa",
                "https://blockstream.info/block/0000aa",
            ])],
            &views,
        );
        let item = &both["items"][0];
        assert_eq!(item["taggers_count"], 2);
        assert_eq!(item["at_least"], true);
        assert_eq!(
            item["tags"],
            json!([{"label": "halving", "taggers_count": 2, "viewer_tagged": false, "at_least": true}])
        );
        let one = social_answer(&[ask(&["https://mempool.space/block/0000aa"])], &views);
        assert_eq!(one["items"][0]["taggers_count"], 2);
        assert!(one["items"][0].get("at_least").is_none());
        assert!(one["items"][0]["tags"][0].get("at_least").is_none());
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
