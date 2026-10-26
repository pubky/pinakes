//! Pinakes for agents, on the public listener: the usage guide (`GET /llms.txt`, and as a skill,
//! `GET /skill.md`), an index that links them (`GET /`), and the MCP endpoint (`POST /mcp`,
//! [`mcp`]). The guide names the host that served it as the base URL, as nexus-scout's does.
use crate::api::AppState;
use axum::{
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use utoipa::ToSchema;

pub mod mcp;

/// The guide, with `{{BASE}}` where the base URL goes.
const GUIDE: &str = include_str!("guide.md");
/// The skill's front matter: what an agent reads to decide when to load it.
const SKILL_FRONT_MATTER: &str = "---\nname: pinakes\ndescription: Use when you need facts about the external links people share on Pubky - what a link is (its typed card), places near a point with a tag, events in a city and time window, every link with one ISBN, DOI, ISRC or OSM id, and which links are the same thing on several services. Read-only HTTP search over Pinakes; join with Nexus by URI for tags and taggers.\n---\n\n";

/// The routes of the guide and the index.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/llms.txt", get(llms_txt))
        .route("/skill.md", get(skill_md))
}

/// The base URL of the request: its `Host`, with `X-Forwarded-Proto` when a proxy names the
/// scheme, else `http` for a loopback host and `https` for any other. A `Host` that is not a
/// host name (or absent) gives `http://localhost`.
pub fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 255
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        });
    let Some(host) = host else {
        return "http://localhost".into();
    };
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|v| v == "http" || v == "https");
    let scheme = forwarded.unwrap_or_else(|| {
        let name = host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                name
            } else {
                host
            }
        });
        if name == "localhost" || name.starts_with("127.") || name == "[::1]" {
            "http".into()
        } else {
            "https".into()
        }
    });
    format!("{scheme}://{host}")
}

/// The guide for the host that asked.
pub fn guide(base: &str) -> String {
    GUIDE.replace("{{BASE}}", base)
}

fn text(content_type: &'static str, body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=300"),
            ),
            (
                header::VARY,
                HeaderValue::from_static("host, x-forwarded-proto"),
            ),
        ],
        body,
    )
        .into_response()
}

#[utoipa::path(get,tag="Agents",path="/llms.txt",responses((status=200,content_type="text/plain",body=String,description="The usage guide for agents: what Pinakes indexes, a quickstart, recipes, joining with Nexus, MCP, limits and errors. Its base URL is the host that served it.")))]
pub async fn llms_txt(headers: HeaderMap) -> Response {
    text("text/plain; charset=utf-8", guide(&base_url(&headers)))
}

#[utoipa::path(get,tag="Agents",path="/skill.md",responses((status=200,content_type="text/markdown",body=String,description="The guide as an agent skill: front matter named `pinakes`, then the guide.")))]
pub async fn skill_md(headers: HeaderMap) -> Response {
    text(
        "text/markdown; charset=utf-8",
        format!("{SKILL_FRONT_MATTER}{}", guide(&base_url(&headers))),
    )
}

/// `GET /`: where an agent starts.
#[derive(Serialize, ToSchema)]
pub struct Index {
    pub service: String,
    pub description: String,
    /// The usage guide: read it first.
    pub start_here: String,
    pub links: IndexLinks,
}
#[derive(Serialize, ToSchema)]
pub struct IndexLinks {
    pub guide: String,
    pub skill: String,
    /// The MCP endpoint; absent when `api.mcp` is off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<String>,
    pub openapi: String,
    pub info: String,
    /// Graph questions (who follows whom, trust, threads): nexus-scout.
    pub social_graph: String,
}

#[utoipa::path(get,tag="Agents",path="/",responses((status=200,body=Index)),description="Where an agent starts: the guide, the skill, the MCP endpoint, the OpenAPI document and nexus-scout for graph questions.")]
pub async fn index(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
) -> Json<Index> {
    let base = base_url(&headers);
    Json(Index {
        service: "pinakes".into(),
        description: "Read-only search over the external links people reference on Pubky: typed cards, places, events, identifiers and groups.".into(),
        start_here: format!("{base}/llms.txt"),
        links: IndexLinks {
            guide: format!("{base}/llms.txt"),
            skill: format!("{base}/skill.md"),
            mcp: state.config.api.mcp.then(|| format!("{base}/mcp")),
            openapi: format!("{base}/v1/openapi.json"),
            info: format!("{base}/v1/info"),
            social_graph: "https://nexus-scout.pubky.app/llms.txt".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn the_base_is_the_host_that_asked() {
        for (pairs, base) in [
            (vec![("host", "pinakes.example")], "https://pinakes.example"),
            (vec![("host", "localhost:8090")], "http://localhost:8090"),
            (vec![("host", "127.0.0.1:8090")], "http://127.0.0.1:8090"),
            (vec![("host", "[::1]:8090")], "http://[::1]:8090"),
            (
                vec![("host", "pinakes.example"), ("x-forwarded-proto", "http")],
                "http://pinakes.example",
            ),
            (
                vec![("host", "localhost"), ("x-forwarded-proto", "HTTPS, http")],
                "https://localhost",
            ),
            (
                vec![("host", "pinakes.example"), ("x-forwarded-proto", "gopher")],
                "https://pinakes.example",
            ),
            (vec![("host", "evil.example/<script>")], "http://localhost"),
            (vec![], "http://localhost"),
        ] {
            assert_eq!(base_url(&headers(&pairs)), base, "{pairs:?}");
        }
    }

    #[test]
    fn the_guide_names_its_base_everywhere() {
        let guide = guide("https://pinakes.example");
        assert!(!guide.contains("{{"));
        assert!(guide.contains("curl -s 'https://pinakes.example/v1/search?q=lugano&limit=5'"));
        assert!(guide.contains("POST https://pinakes.example/mcp"));
        assert!(guide.contains("https://nexus-scout.pubky.app/llms.txt"));
        assert!(SKILL_FRONT_MATTER.starts_with("---\nname: pinakes\ndescription: "));
        assert!(SKILL_FRONT_MATTER.ends_with("---\n\n"));
    }
}
