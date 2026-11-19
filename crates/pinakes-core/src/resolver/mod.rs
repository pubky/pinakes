use crate::model::ResourceMeta;
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

pub mod cards;
pub mod egress;
pub mod geo;
pub mod html;
pub mod interstitial;
pub mod osm;
pub mod pkarr_relays;
pub mod pubky;
pub mod pubky_apps;
pub mod web;

#[async_trait]
pub trait Resolver: Send + Sync {
    fn manifest(&self) -> crate::adapter::Manifest {
        crate::adapter::manifest(self.name())
    }
    fn manifest_for(&self, _uri: &str) -> crate::adapter::Manifest {
        self.manifest()
    }
    fn manifests(&self) -> Vec<crate::adapter::Manifest> {
        vec![self.manifest()]
    }
    fn canary_allowed_for(&self, manifest: &crate::adapter::Manifest) -> bool {
        manifest.enabled && self.canary_allowed()
    }
    /// The link a canary reads: the manifest's own sample.
    async fn canary_target(&self, manifest: &crate::adapter::Manifest) -> Result<String> {
        manifest
            .canary
            .as_ref()
            .map(|u| u.to_string())
            .ok_or_else(|| anyhow::anyhow!("no canary"))
    }
    async fn probe_manifest(
        &self,
        uri: &str,
        _manifest: &crate::adapter::Manifest,
    ) -> Result<ResourceMeta> {
        self.probe(uri).await
    }
    async fn probe(&self, uri: &str) -> Result<ResourceMeta> {
        self.resolve(uri).await
    }
    fn name(&self) -> &'static str;
    fn queue_sources(&self) -> Vec<&'static str> {
        vec![self.name()]
    }
    fn matches(&self, uri: &str) -> bool;
    fn canary_allowed(&self) -> bool {
        true
    }
    async fn resolve(&self, uri: &str) -> Result<ResourceMeta>;
    fn batch_size(&self) -> usize {
        1
    }
    async fn resolve_batch(&self, uris: &[String]) -> Vec<Result<ResourceMeta>> {
        let mut out = Vec::with_capacity(uris.len());
        for uri in uris {
            out.push(self.resolve(uri).await);
        }
        out
    }
    fn refresh_after(&self, doc: &ResourceMeta) -> Duration;
}
/// Every route `classify` returns besides the card kinds (`cards::KINDS`). `classify` returns
/// only these constants, so a new route is added here.
pub const ROUTES: [&str; 6] = ["web", "pubky", "geo", "osm", "event", "event_platform"];
const WEB: &str = ROUTES[0];
const PUBKY: &str = ROUTES[1];
const GEO: &str = ROUTES[2];
const OSM: &str = ROUTES[3];
const EVENT: &str = ROUTES[4];
const EVENT_PLATFORM: &str = ROUTES[5];

/// The app of a `pubky://` object (`/pub/{app}/…`), its path decoded as a homeserver decodes it.
/// `None` outside `/pub/`, for an empty app, and for any segment that decodes to a separator or
/// a dot segment: the homeserver would read another path than the one classified.
pub(crate) fn pubky_app(url: &url::Url) -> Option<String> {
    match path_segments(url)?.as_slice() {
        [public, app, ..] if public == "pub" && !app.is_empty() => Some(app.clone()),
        _ => None,
    }
}
/// A `pubky://` object's path segments, decoded as a homeserver decodes them; `None` when one
/// decodes to a separator or a dot segment.
pub(crate) fn path_segments(url: &url::Url) -> Option<Vec<String>> {
    let segments: Vec<String> = url
        .path()
        .split('/')
        .skip(1)
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .ok()
                .map(|s| s.into_owned())
        })
        .collect::<Option<_>>()?;
    if segments
        .iter()
        .any(|s| s.contains(['/', '\\']) || s == "." || s == "..")
    {
        return None;
    }
    Some(segments)
}
/// Scheme modules and host adapters go before the generic web fallback.
pub fn classify(uri: &str) -> Option<&'static str> {
    let route = classify_route(uri);
    debug_assert!(route.is_none_or(|r| ROUTES.contains(&r) || cards::KINDS.contains(&r)));
    route
}
fn classify_route(uri: &str) -> Option<&'static str> {
    let url = url::Url::parse(uri).ok()?;
    if let Some(kind) = cards::classify(uri) {
        return Some(kind);
    }
    match url.scheme() {
        // Another app's object: a kind its profile reads, or any object of an app no profile
        // knows (`pubky_apps::route`).
        "pubky"
            if pubky_app(&url).is_some()
                && path_segments(&url)
                    .and_then(|s| pubky_apps::route(&s[1..]))
                    .is_some_and(|r| !matches!(r, pubky_apps::Route::Excluded)) =>
        {
            Some(PUBKY)
        }
        "http" | "https"
            if url.host_str() == Some("api.nostr.wine")
                && crate::event::adapters::detect(uri).is_none() =>
        {
            None
        }
        "http" | "https" if osm::parse_id(uri).is_some() => Some(OSM),
        // Map links that name a point; never identity, so they never group.
        "http" | "https" if geo::location_link(uri).is_some() => Some(GEO),
        "nostr" => Some(EVENT_PLATFORM),
        "http" | "https" | "webcal" | "webcals"
            if crate::event::adapters::detect(uri).is_some_and(|d| d.id != "ics") =>
        {
            Some(EVENT_PLATFORM)
        }
        "webcal" | "webcals" => Some(EVENT),
        "http" | "https" if crate::event::service::is_feed(uri) => Some(EVENT),
        "http" | "https" => Some(WEB),
        "geo" => Some(GEO),
        _ => None,
    }
}

/// Persisted routing hints are configuration-independent; disabled calendars use web workers.
pub fn classify_config(uri: &str, config: &crate::config::Config) -> Option<&'static str> {
    match classify(uri) {
        Some(kind)
            if cards::KINDS.contains(&kind)
                && (!config.cards.resolver(kind).enabled
                    || (kind == "official"
                        && crate::sources::enabled_source(uri, config).is_none())) =>
        {
            if url::Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "http" | "https")) {
                Some("web")
            } else {
                None
            }
        }
        Some("event") if !config.event.enabled || !config.event.calendars => Some("web"),
        Some("event_platform")
            if crate::event::adapters::detect(uri)
                .is_some_and(|d| !crate::event::adapters::manifest(d, &config.event).enabled)
                && url::Url::parse(uri).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https" | "webcal" | "webcals")
                }) =>
        {
            Some("web")
        }
        other => other,
    }
}

/// The route `POST /v1/resolve` takes for `uri` under `config`, or the reason it answers 400: no
/// resolver reads the URI, or its resolver is switched off.
pub fn admit(uri: &str, config: &crate::config::Config) -> Result<&'static str, &'static str> {
    let resolver = classify_config(uri, config).ok_or("no resolver available for this URI")?;
    if (resolver == EVENT_PLATFORM
        && crate::event::adapters::detect(uri)
            .is_none_or(|d| !crate::event::adapters::manifest(d, &config.event).enabled))
        || (resolver == WEB && !config.web.enabled)
        || (resolver == PUBKY && !config.pubky.enabled)
        || (resolver == GEO && !config.geo.enabled)
        || (resolver == OSM && !config.osm.enabled)
        || (resolver == EVENT && (!config.event.enabled || !config.event.calendars))
    {
        return Err("resolver is disabled");
    }
    Ok(resolver)
}

#[derive(Debug)]
pub struct NotModified;
impl std::fmt::Display for NotModified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream not modified")
    }
}
impl std::error::Error for NotModified {}

#[derive(Clone, Debug)]
pub struct RetryLater {
    pub class: &'static str,
    pub seconds: u64,
}
impl std::fmt::Display for RetryLater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; retry later", self.class)
    }
}
impl std::error::Error for RetryLater {}

#[derive(Clone, Copy, Debug)]
pub struct ErrorClass(pub &'static str);
impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ErrorClass {}
/// The part of a link a log names: its host, or its scheme for a link without one (`pubky`,
/// `doi`, `isbn`). Never its path or query, which may carry what the link's owner keeps private.
pub fn log_host(uri: &str) -> String {
    url::Url::parse(uri)
        .map(|u| u.host_str().unwrap_or(u.scheme()).to_owned())
        .unwrap_or_default()
}

/// The URL schemes `redact` cuts: the web's, and the database's (a `DATABASE_URL` holds a
/// password as user info and the socket or TLS settings as its query).
const REDACTED_SCHEMES: [&str; 4] = ["http://", "https://", "postgres://", "postgresql://"];

/// `text` with every web or database URL in it cut before its query or fragment (from the first
/// `?` or `#` to the next white space) and without user info, in any letter case. The last guard
/// for error text that goes to a log; no code puts a URL in an error on purpose.
pub fn redact(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some((start, scheme)) = REDACTED_SCHEMES
        .iter()
        .filter_map(|scheme| lower[at..].find(scheme).map(|i| (at + i, scheme.len())))
        .min()
    {
        out.push_str(&text[at..start]);
        // The link ends at white space, or where another link begins.
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| start + n);
        let end = REDACTED_SCHEMES
            .iter()
            .filter_map(|s| lower[start + scheme..end].find(s))
            .min()
            .map_or(end, |n| start + scheme + n);
        let link = &text[start..end];
        // Up to the first `?` or `#`: no query, no fragment.
        let cut = link.find(['?', '#']).unwrap_or(link.len());
        let (head, rest) = link[..cut].split_at(scheme);
        // No user info: what precedes an `@` in the authority.
        let authority = rest.find('/').unwrap_or(rest.len());
        let rest = match rest[..authority].rfind('@') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        out.push_str(head);
        out.push_str(rest);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// An error as a log line prints it: its whole chain (`{:#}`), through `redact`. Only the lines
/// about this process and its own services (the database, Nexus, the dump) print an error; a
/// line about a fetch, a lookup or a parse prints the failure's kind (`error_class`), its
/// upstream status (`status_of`) and the host asked, never an error's text. Tests hold both.
pub fn log_error<E: std::fmt::Display + ?Sized>(error: &E) -> String {
    redact(&format!("{error:#}"))
}

/// The HTTP status an upstream answered, as an error's context, so a log line can name it.
#[derive(Debug)]
pub struct Upstream(pub u16);
impl std::fmt::Display for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream answered HTTP {}", self.0)
    }
}

/// The HTTP status behind a failure, when one was answered.
pub fn status_of(error: &anyhow::Error) -> Option<u16> {
    error.downcast_ref::<Upstream>().map(|u| u.0).or_else(|| {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<reqwest::Error>()?.status())
            .map(|status| status.as_u16())
    })
}

pub fn error_class(error: &anyhow::Error) -> &'static str {
    if error.is::<interstitial::Blocked>() {
        return "wall";
    }
    if let Some(class) = error.downcast_ref::<ErrorClass>() {
        return class.0;
    }
    if let Some(retry) = error.downcast_ref::<RetryLater>() {
        return retry.class;
    }
    if error.is::<tokio::time::error::Elapsed>() {
        return "timeout";
    }
    if error.chain().any(|cause| cause.is::<Unreached>()) {
        return "connect";
    }
    // An answer past its size cap: the page is readable, only too large.
    if error
        .chain()
        .any(|cause| cause.is::<egress::BodyTooLarge>())
    {
        return "too_large";
    }
    // A parse that did not end within its limits: no card, and no retry.
    if error.chain().any(|cause| cause.is::<html::TooComplex>()) {
        return "too_complex";
    }
    for cause in error.chain() {
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            if request.is_timeout() {
                return "timeout";
            }
            if request.is_connect() || transport(request) {
                return "connect";
            }
            if let Some(status) = request.status() {
                return if status.is_server_error() {
                    "http_5xx"
                } else {
                    "http_4xx"
                };
            }
        }
    }
    "parse"
}

/// A connection that never reached the upstream, as an error rebuilt without its cause carries
/// it (`egress::Egress::fetch_with` for a request with credentials, whose cause may name them).
#[derive(Debug, Clone, Copy)]
pub struct Unreached;
impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection to the upstream failed")
    }
}
impl std::error::Error for Unreached {}

/// Whether a request failed in the network under it, not in an answer: the connection was
/// refused, reset or aborted, the pipe broke, or the host or its network is unreachable, while
/// connecting, sending the request or reading the answer. Not a timeout (it has its own grace,
/// `limits.timeout_grace`), and not a TLS failure (a certificate the host serves is an answer
/// about it).
pub(crate) fn transport(request: &reqwest::Error) -> bool {
    if request.is_timeout() {
        return false;
    }
    let mut source = std::error::Error::source(request);
    while let Some(cause) = source {
        if cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            use std::io::ErrorKind::*;
            matches!(
                io.kind(),
                ConnectionRefused
                    | ConnectionReset
                    | ConnectionAborted
                    | BrokenPipe
                    | NotConnected
                    | HostUnreachable
                    | NetworkUnreachable
            )
        }) {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Whether a failure never reached an answer from the upstream (`transport`, or `Unreached`
/// where the error was rebuilt without its cause). Such a failure says nothing about the link,
/// so the queue never dead-letters on it. A `connect` class a resolver names itself (a PKARR
/// relay that serves no usable packet, a homeserver with only key-pinned endpoints) is an answer
/// about the link and is not one; nor is a TLS failure or a timeout.
pub fn unreached(error: &anyhow::Error) -> bool {
    error_class(error) == "connect"
        && error.chain().any(|cause| {
            cause.is::<Unreached>()
                || cause
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(transport)
        })
}

#[cfg(test)]
mod tests {
    /// A logged error never carries what a URL's query, fragment or user info holds, whatever
    /// punctuation follows the URL and whatever the scheme's letter case.
    #[test]
    fn redact_keeps_no_query_fragment_or_user_info() {
        for (text, want) in [
            (
                "error sending request for url (https://cal.example.org/calendar.ics?token=CANARY#x)",
                "error sending request for url (https://cal.example.org/calendar.ics",
            ),
            (
                "https://example.org/cal?x=1,token=CANARY#FRAGMENT then more",
                "https://example.org/cal then more",
            ),
            ("HTTPS://Example.org/a?token=CANARY", "HTTPS://Example.org/a"),
            ("at https://user:CANARY@example.org/a, ok", "at https://example.org/a, ok"),
            // Two links in one span: the second loses its user info too.
            (
                "failed [https://first.example/a,HTTPS://user:CANARY@second.example/b?t=CANARY]",
                "failed [https://first.example/a,HTTPS://second.example/b",
            ),
            ("from http://example.org/c#CANARY", "from http://example.org/c"),
            // A database URL: its password (user info) and its settings (query) go.
            (
                "connect postgresql://pinakes:CANARY@db.example:5432/pinakes?sslmode=require&password=CANARY failed",
                "connect postgresql://db.example:5432/pinakes failed",
            ),
            (
                "POSTGRES://user:CANARY@/pinakes?host=/run/CANARY",
                "POSTGRES:///pinakes",
            ),
            (
                "url=postgres://u:CANARY@db/x,then https://user:CANARY@a.example/b",
                "url=postgres://db/x,then https://a.example/b",
            ),
            ("no link here", "no link here"),
            ("https://example.org/plain", "https://example.org/plain"),
        ] {
            assert_eq!(redact(text), want, "{text}");
            assert!(!redact(text).contains("CANARY"));
        }
        assert_eq!(
            log_host("https://user:CANARY@cal.example.org/a?token=CANARY"),
            "cal.example.org"
        );
        assert_eq!(log_host("doi:10.1000/x?token=CANARY"), "doi");
    }

    /// `text` with each string literal's contents removed.
    fn strip_strings(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '"' {
                let mut escaped = false;
                for d in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == '"' {
                        break;
                    }
                }
                out.push_str("\"\"");
            } else {
                out.push(c);
            }
        }
        out
    }

    /// `text` without the calls to `name(…)`, parentheses balanced.
    fn strip_calls(text: &str, name: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(i) = rest.find(name) {
            out.push_str(&rest[..i]);
            let mut depth = 0usize;
            let mut end = rest.len();
            for (j, c) in rest[i + name.len() - 1..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + name.len() - 1 + j + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// Every logging macro call in the code (not the tests) of this crate and the daemon's: its
    /// file and its text from the macro's name to its closing parenthesis.
    fn log_calls() -> Vec<(std::path::PathBuf, String)> {
        let core = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![core.join("src"), core.join("../pinakesd/src")];
        let mut calls = vec![];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.split("#[cfg(test)]").next().unwrap_or("");
                let mut rest = code;
                while let Some(i) = rest.find("tracing::") {
                    let after = &rest[i..];
                    let Some(open) = after.find("!(") else { break };
                    let mut depth = 0usize;
                    let mut end = after.len();
                    for (j, c) in after[open + 1..].char_indices() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = open + 1 + j + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    calls.push((path.clone(), after[..end].to_owned()));
                    rest = &after[end.max(1)..];
                }
            }
        }
        assert!(calls
            .iter()
            .any(|(p, _)| p.to_string_lossy().contains("pinakesd")));
        assert!(calls.len() > 40, "the scan found {} log calls", calls.len());
        calls
    }

    /// No log line names a link: a logging macro mentions `uri`, `url` or `link` only inside
    /// `log_host(…)` or `redact(…)`.
    #[test]
    fn no_log_line_names_a_link() {
        let mut found = vec![];
        for (path, call) in log_calls() {
            let plain = strip_calls(&strip_calls(&strip_strings(&call), "log_host("), "redact(");
            let words: Vec<&str> = plain
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .collect();
            if words.iter().any(|w| matches!(*w, "uri" | "url" | "link")) {
                found.push(format!(
                    "{}: {}",
                    path.display(),
                    call.lines().next().unwrap_or("")
                ));
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// Every log line that prints an error prints it through `log_error(…)`: an error's text may
    /// hold a URL (a client library names the one it asked), and `log_error` redacts it. Outside
    /// `log_error`, `log_host`, `error_class` and `status_of` calls, a log call names `e`, `err`,
    /// `error` or `cause` only as a field's name, and no format string prints one.
    #[test]
    fn every_logged_error_goes_through_log_error() {
        let mut found = vec![];
        for (path, call) in log_calls() {
            // From the macro's arguments on: `tracing::error!` names itself.
            let args = &call[call.find("!(").unwrap() + 2..];
            let printed = [
                "{e}", "{e:", "{err}", "{err:", "{error}", "{error:", "{cause",
            ]
            .iter()
            .any(|p| args.contains(p));
            let mut plain = strip_strings(args);
            for name in ["log_error(", "log_host(", "error_class(", "status_of("] {
                plain = strip_calls(&plain, name);
            }
            let named = plain
                .match_indices(|c: char| c.is_ascii_alphabetic())
                .any(|(i, _)| {
                    let starts = i == 0 || {
                        let b = plain.as_bytes()[i - 1];
                        !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b':')
                    };
                    if !starts {
                        return false;
                    }
                    let word: String = plain[i..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    let after = plain[i + word.len()..].trim_start();
                    let field_name = after.starts_with('=') && !after.starts_with("==");
                    matches!(word.as_str(), "e" | "err" | "error" | "cause") && !field_name
                });
            if printed || named {
                found.push(format!(
                    "{}: {}",
                    path.display(),
                    call.lines().next().unwrap_or("")
                ));
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    /// No log line on a fetch, lookup or parse path prints an error's text, not even through
    /// `log_error`: what an upstream answered may hold a link. Those lines print the failure's
    /// kind (`error_class`). The lines about this process's own services may print theirs.
    #[test]
    fn no_fetch_path_logs_an_error_s_text() {
        let own = [
            "/api.rs",
            "/groups.rs",
            "/maintenance.rs",
            "/osm_dump.rs",
            "/queue.rs",
            "/same_event.rs",
            "/same_recording.rs",
            "/resolver/mod.rs",
            "pinakesd/src/main.rs",
        ];
        let mut found = vec![];
        for (path, call) in log_calls() {
            let name = path.to_string_lossy();
            if own.iter().any(|o| name.ends_with(o)) {
                continue;
            }
            if call.contains("log_error(") {
                found.push(format!("{name}: {}", call.lines().next().unwrap_or("")));
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn log_error_prints_the_chain_redacted() {
        let error =
            anyhow::anyhow!("error sending request for url (https://u:p@x.example/a?token=CANARY)")
                .context("Nexus feed");
        let line = log_error(&error);
        assert!(
            line.starts_with("Nexus feed: error sending request"),
            "{line}"
        );
        assert!(line.contains("https://x.example/a"), "{line}");
        assert!(!line.contains("CANARY") && !line.contains("u:p"), "{line}");
    }

    /// A body past its cap is `too_large`, however deep in the error chain (an adapter adds
    /// context), never `parse`, which would hide the cause.
    #[test]
    fn an_answer_past_its_cap_is_too_large() {
        let bare = anyhow::Error::new(egress::BodyTooLarge {
            cap: egress::BODY_CAP,
        });
        assert_eq!(error_class(&bare), "too_large");
        let wrapped = anyhow::Error::new(egress::BodyTooLarge {
            cap: egress::BODY_CAP,
        })
        .context("Wikidata item")
        .context("event adapter");
        assert_eq!(error_class(&wrapped), "too_large");
        assert_eq!(error_class(&anyhow::anyhow!("no title")), "parse");
    }

    use super::*;

    #[test]
    fn every_route_is_reachable() {
        // Every route is reachable, so ROUTES lists nothing classify cannot return.
        let samples = [
            "https://example.org/page",
            "https://www.openstreetmap.org/node/1",
            "https://osm.org/go/0EEQjE--",
            "geo:46.0,8.9",
            "webcal://example.org/calendar.ics",
            "nostr:naddr1qqxnzd3cxqmrzv3exgmr2wfeqgsxu35yyt0mwjjh8pcz4zprhxegz69t4wr9t74vk6zne58wzh0waycrqsqqqa28pccpzu",
            "pubky://o1gg96ewuojmopcjbz8895478wdtxtzzber7aezq6ror5a91j7dy/pub/example.app/x",
        ];
        let routed: std::collections::BTreeSet<_> =
            samples.iter().filter_map(|uri| classify(uri)).collect();
        assert_eq!(routed, ROUTES.into_iter().collect());
    }
}
