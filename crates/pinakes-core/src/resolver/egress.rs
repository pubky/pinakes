use crate::config::EgressConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    time::Instant,
};
use url::{Host, Url};

/// A body over the cap its read allows (`cap` bytes): refused, never cut.
#[derive(Clone, Copy, Debug)]
pub struct BodyTooLarge {
    pub cap: usize,
}
impl std::fmt::Display for BodyTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.cap >= 1024 * 1024 && self.cap.is_multiple_of(1024 * 1024) {
            write!(f, "body exceeds its cap of {} MB", self.cap / (1024 * 1024))
        } else {
            write!(f, "body exceeds its cap of {} bytes", self.cap)
        }
    }
}
impl std::error::Error for BodyTooLarge {}

/// A request that had its host slot and was not answered within its caller's deadline
/// ([`FetchOptions::request_deadline`]).
#[derive(Clone, Copy, Debug)]
pub struct RequestDeadline(pub Duration);
impl std::fmt::Display for RequestDeadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no answer within {} ms", self.0.as_millis())
    }
}
impl std::error::Error for RequestDeadline {}
/// Whether `error` is, or wraps, a body over its read's cap.
pub fn over_cap(error: &anyhow::Error) -> bool {
    error.chain().any(|c| c.is::<BodyTooLarge>())
}

pub const BODY_CAP: usize = 1024 * 1024;
/// The longest a request waits for its host's next slot, queue included. It leaves the request
/// at least half of the ten-second fetch timeout.
pub const MAX_SLOT_WAIT: Duration = Duration::from_secs(5);
/// The Fetch standard's bad ports (<https://fetch.spec.whatwg.org/#port-blocking>): ports of
/// services other than the web (mail, SSH, DNS, IRC, …). No request or relay connection is made
/// to one, whoever names it (a link, a redirect, a homeserver's PKARR record).
pub const BAD_PORTS: &[u16] = &[
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667,
    6668, 6669, 6679, 6697, 10080,
];
pub const USER_AGENT: &str = concat!(
    "pinakes/",
    env!("CARGO_PKG_VERSION"),
    " (+https://pubky.app)"
);
/// The User-Agent with the operator's contact (`http.contact`), or `USER_AGENT` without one.
/// It never names a code host: some services refuse such agents as undeclared tools.
pub fn user_agent(contact: &str) -> String {
    if contact.is_empty() {
        USER_AGENT.to_owned()
    } else {
        format!(
            "pinakes/{} (+https://pubky.app; {contact})",
            env!("CARGO_PKG_VERSION")
        )
    }
}
pub const ALLOWED_TYPES: &[&str] = &[
    "text/html",
    "application/xhtml+xml",
    "application/json",
    "application/ld+json",
    "application/activity+json",
    "application/vnd.citationstyles.csl+json",
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
    "image/avif",
    "application/pdf",
];

/// What a fetch asks for when its caller names nothing: the allowed types, HTML first; for a link
/// that looks like a feed (its last segment ends `.atom`, `.rss`, `.rdf` or `.xml`, or is named
/// `feed`, `rss` or `atom`, with any extension), the feed types first. Some sites answer a feed's
/// own URL with a 404 when HTML leads (jsonfeed.org's `feed.json`).
pub fn default_accept(url: &Url) -> String {
    let last = url
        .path_segments()
        .and_then(|mut s| s.rfind(|s| !s.is_empty()))
        .unwrap_or("")
        .to_ascii_lowercase();
    let (stem, extension) = last.rsplit_once('.').unwrap_or((&last, ""));
    if matches!(extension, "atom" | "rss" | "rdf" | "xml")
        || matches!(stem, "feed" | "rss" | "atom")
    {
        "application/atom+xml, application/rss+xml, application/feed+json, */*;q=0.5".into()
    } else {
        ALLOWED_TYPES.join(", ")
    }
}

pub fn public_ip(ip: IpAddr) -> bool {
    static DENIED: OnceLock<Vec<ipnet::IpNet>> = OnceLock::new();
    let denied = DENIED.get_or_init(|| {
        [
            "0.0.0.0/8",
            "10.0.0.0/8",
            "100.64.0.0/10",
            "127.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.0.0.0/24",
            "192.0.2.0/24",
            "192.88.99.0/24",
            "192.168.0.0/16",
            "198.18.0.0/15",
            "198.51.100.0/24",
            "203.0.113.0/24",
            "224.0.0.0/4",
            "240.0.0.0/4",
            "168.63.129.16/32",
            "2001::/23",
            "2001:db8::/32",
            "2002::/16",
            "3fff::/20",
        ]
        .iter()
        .map(|s| s.parse().expect("CIDR"))
        .collect()
    });
    let ip = match ip {
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                // Excludes local, multicast, unspecified, NAT64 and other transition ranges.
                if (v.segments()[0] & 0xe000) != 0x2000 {
                    return false;
                }
                IpAddr::V6(v)
            }
        }
        v => v,
    };
    !denied.iter().any(|net| net.contains(&ip))
}
#[derive(Clone)]
pub struct Fetched {
    /// True only when every redirect from the requested URI was HTTP 301/308.
    pub permanent_redirect: bool,
    pub truncated: bool,
    pub validators: Validators,
    pub url: Url,
    pub content_type: String,
    /// The `charset` parameter of the answer's `Content-Type`, lower-cased, when it named one
    /// ([`Fetched::text`] reads the body by it).
    pub charset: Option<String>,
    pub status: u16,
    pub retry_after_secs: u64,
    pub location: Option<String>,
    pub body: Vec<u8>,
}
#[cfg(test)]
impl Fetched {
    /// An answer as a test's transport gives it: `status` with `body` of `content_type` at
    /// `url`, whole, with no redirect, no validators, no `Location` and a retry-after of 60 s.
    /// A test that needs another value sets that field with struct update syntax.
    pub(crate) fn answer(url: Url, status: u16, content_type: String, body: Vec<u8>) -> Self {
        Self {
            permanent_redirect: false,
            truncated: false,
            validators: Validators::default(),
            url,
            content_type,
            charset: None,
            status,
            retry_after_secs: 60,
            location: None,
            body,
        }
    }
}
impl Fetched {
    /// The body as text, decoded once ([`decode`]) by the charset the answer or the page names.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        decode(&self.body, self.charset.as_deref())
    }
}
/// `body` as text in its character encoding: a byte order mark's, else `charset` (the
/// `Content-Type` parameter), else one a `<meta charset>` or `<meta http-equiv>` declares in the
/// first 1024 bytes, else UTF-8. Bytes the encoding cannot read become U+FFFD.
pub fn decode<'a>(body: &'a [u8], charset: Option<&str>) -> std::borrow::Cow<'a, str> {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(body) {
        return encoding.decode_without_bom_handling(&body[bom..]).0;
    }
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .or_else(|| meta_charset(&body[..body.len().min(1024)]))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode_without_bom_handling(body).0
}
/// The encoding a `<meta>` among `head` declares, read as the HTML standard's prescan does: a
/// tag's own `charset` attribute, or the `charset=` in the `content` of one whose `http-equiv` is
/// `Content-Type`. Comments and other tags (their attributes too) are passed over, so a
/// `data-charset`, a `content` without the `http-equiv`, or a `<meta>` in a comment or in an
/// attribute's value declares nothing. As browsers read it, a declared UTF-16 is UTF-8 (the
/// declaration was readable as ASCII) and `x-user-defined` is windows-1252.
fn meta_charset(head: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let starts = |at: usize, prefix: &[u8]| {
        head.get(at..at + prefix.len())
            .is_some_and(|w| w.eq_ignore_ascii_case(prefix))
    };
    let mut at = 0;
    while at < head.len() {
        if starts(at, b"<!--") {
            // A comment ends at the first `-->` after its opening (`<!-->` too).
            let from = at + 2;
            at = head[from..]
                .windows(3)
                .position(|w| w == b"-->")
                .map_or(head.len(), |n| from + n + 3);
        } else if starts(at, b"<meta")
            && head
                .get(at + 5)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'/')
        {
            at += 5;
            let mut seen: Vec<Vec<u8>> = Vec::new();
            let (mut charset, mut pragma, mut need_pragma) = (None, false, None);
            // A `charset` attribute decides, even with a label no encoding has.
            let mut charset_attribute = false;
            while let Some((name, value)) = attribute(head, &mut at) {
                if seen.contains(&name) {
                    continue;
                }
                match name.as_slice() {
                    b"http-equiv" => pragma = value.eq_ignore_ascii_case(b"content-type"),
                    b"content" if charset.is_none() && !charset_attribute => {
                        if let Some(label) = content_charset(&value) {
                            charset = encoding_rs::Encoding::for_label(label);
                            need_pragma = Some(true);
                        }
                    }
                    b"charset" => {
                        charset_attribute = true;
                        charset = encoding_rs::Encoding::for_label(value.trim_ascii());
                        need_pragma = Some(false);
                    }
                    _ => {}
                }
                seen.push(name);
            }
            match (need_pragma, charset) {
                (Some(need), Some(encoding)) if pragma || !need => {
                    return Some(if encoding == encoding_rs::X_USER_DEFINED {
                        encoding_rs::WINDOWS_1252
                    } else {
                        encoding.output_encoding()
                    });
                }
                _ => {}
            }
        } else if head[at] == b'<' && head.get(at + 1).is_some_and(u8::is_ascii_alphabetic)
            || starts(at, b"</") && head.get(at + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            // Another tag: its name, then its attributes, whose values may hold any `<`.
            at = head[at..]
                .iter()
                .position(|b| b.is_ascii_whitespace() || *b == b'>')
                .map_or(head.len(), |n| at + n);
            while attribute(head, &mut at).is_some() {}
        } else if starts(at, b"<!") || starts(at, b"</") || starts(at, b"<?") {
            at = head[at..]
                .iter()
                .position(|b| *b == b'>')
                .map_or(head.len(), |n| at + n + 1);
        } else {
            at += 1;
        }
    }
    None
}
/// The next attribute of a tag in `head` from `at` (the standard's "get an attribute"): its name
/// in lower case and its value, `at` moved past it; `None` at the tag's end (`at` past its `>`)
/// or the end of `head`.
fn attribute(head: &[u8], at: &mut usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let byte = |at: usize| head.get(at).copied();
    while byte(*at).is_some_and(|b| b.is_ascii_whitespace() || b == b'/') {
        *at += 1;
    }
    if byte(*at)? == b'>' {
        *at += 1;
        return None;
    }
    let mut name = Vec::new();
    loop {
        match byte(*at)? {
            b'=' if !name.is_empty() => break,
            b if b.is_ascii_whitespace() => break,
            b'/' | b'>' => return Some((name, Vec::new())),
            b => name.push(b.to_ascii_lowercase()),
        }
        *at += 1;
    }
    while byte(*at).is_some_and(|b| b.is_ascii_whitespace()) {
        *at += 1;
    }
    if byte(*at)? != b'=' {
        return Some((name, Vec::new()));
    }
    *at += 1;
    while byte(*at).is_some_and(|b| b.is_ascii_whitespace()) {
        *at += 1;
    }
    let mut value = Vec::new();
    match byte(*at)? {
        quote @ (b'"' | b'\'') => {
            *at += 1;
            loop {
                let b = byte(*at)?;
                *at += 1;
                if b == quote {
                    return Some((name, value));
                }
                value.push(b.to_ascii_lowercase());
            }
        }
        b'>' => Some((name, value)),
        _ => {
            while let Some(b) = byte(*at).filter(|b| !b.is_ascii_whitespace() && *b != b'>') {
                value.push(b.to_ascii_lowercase());
                *at += 1;
            }
            Some((name, value))
        }
    }
}
/// The encoding label in a `<meta>` `content` value (`text/html; charset=shift_jis`), as the
/// standard extracts it: after a `charset` followed by `=`, quoted (closed) or up to a space or
/// `;`.
fn content_charset(content: &[u8]) -> Option<&[u8]> {
    let mut from = 0;
    loop {
        let at = content[from..]
            .windows(7)
            .position(|w| w.eq_ignore_ascii_case(b"charset"))?;
        let rest = content[from + at + 7..].trim_ascii_start();
        let Some(value) = rest.strip_prefix(b"=") else {
            from += at + 7;
            continue;
        };
        let value = value.trim_ascii_start();
        return match value.first() {
            Some(&quote @ (b'"' | b'\'')) => {
                let value = &value[1..];
                value
                    .iter()
                    .position(|b| *b == quote)
                    .map(|end| &value[..end])
            }
            _ => {
                let end = value
                    .iter()
                    .position(|b| b.is_ascii_whitespace() || *b == b';')
                    .unwrap_or(value.len());
                (end > 0).then(|| &value[..end])
            }
        };
    }
}
/// The seconds until a spent quota is reset, 1 s to an hour: the `X-RateLimit-Reset` time (Unix
/// seconds) of an answer whose `X-RateLimit-Remaining` is 0 (GitHub's rate limit); `None`
/// otherwise.
fn quota_reset(headers: &reqwest::header::HeaderMap, now: std::time::SystemTime) -> Option<u64> {
    let value = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
    if value("x-ratelimit-remaining")? != 0 {
        return None;
    }
    let now = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(
        value("x-ratelimit-reset")?
            .saturating_sub(now)
            .clamp(1, 3600),
    )
}
/// The `charset` parameter of a `Content-Type` value, lower-cased.
fn charset_parameter(value: &str) -> Option<String> {
    value
        .split(';')
        .skip(1)
        .find_map(|parameter| {
            let (name, value) = parameter.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
        })
        .filter(|charset| !charset.is_empty())
}
#[async_trait]
pub trait Dns: Send + Sync {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>>;
}
pub struct SystemDns;
#[async_trait]
impl Dns for SystemDns {
    async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Validators {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
#[derive(Clone, Default)]
pub struct FetchOptions {
    /// Extra request headers, by literal name.
    pub headers: Vec<(&'static str, String)>,
    pub json_body: Option<serde_json::Value>,
    /// A form-encoded POST body (an OAuth token request), by literal field name.
    pub form_body: Option<Vec<(&'static str, String)>>,
    /// Keys the discovery cache in place of the headers when set (a credential pair's hash, so a
    /// rotated bearer token keeps the answers cached under the old one). Never sent.
    pub cache_tag: Option<String>,
    /// Statuses the discovery cache never keeps: an authentication failure belongs to the
    /// credentials sent, not to the URL, and a later request may carry others.
    pub uncached: &'static [u16],
    /// Client errors this caller reads as a wait (GitHub's rate limit answers 403): a
    /// confirmation replays them as it does a 408, a 429 or a 5xx ([`confirming`]).
    pub waits: &'static [u16],
    pub sensitive: bool,
    /// A credential an API takes only in its query (TMDB's v3 `api_key=`), as a parameter name
    /// and value: added to the URL on the wire alone ([`wire_url`]). The URL everything else
    /// sees (the discovery cache's key, the answer's `url`, a redirect, an error, a log) never
    /// holds it. Only on a `sensitive` request.
    pub query_secret: Option<(&'static str, String)>,
    pub accept: Option<String>,
    /// The User-Agent the transport sends: set by the egress to [`user_agent`] with the
    /// operator's contact on every request, whatever a caller put here.
    pub user_agent: Option<String>,
    pub feed_cap: Option<usize>,
    /// How far an RSS or Atom feed read without a feed cap goes before it is cut (marked
    /// truncated, never refused), in place of the page cap: a show's feed read for its episodes
    /// (`episodes::feed_options`). Never less than the page cap; other bodies keep their caps.
    pub feed_read: Option<usize>,
    /// A cap for every body whatever its type, HTML too, in place of the default: a larger body
    /// is refused, never truncated (PKARR relay payloads below the default, Wikidata entities
    /// above it).
    pub body_cap: Option<usize>,
    pub feed_hint: bool,
    /// How far an HTML page is read while its head is still open at the page cap, so a title
    /// after a long head (a megabyte of inline styles) is read. The egress sets it from
    /// `limits.html_head_bytes`.
    pub head_cap: Option<usize>,
    /// Answer a redirect as it came (its status and `location`) instead of following it: where
    /// a lookup redirects is the answer (PCGamingWiki's appid lookup). No redirect of any kind
    /// is followed then: an in-page redirect (a meta refresh, `location.replace(`, a
    /// "Redirecting" stub) comes back as it came too, its status and body, with its target,
    /// resolved against the page, in `location`.
    pub keep_redirect: bool,
    /// An error answer in JSON keeps a bounded prefix of its body, and a 429 is answered as it
    /// came instead of as `RetryLater`: for an API whose error names its own wait (Stack
    /// Exchange's `backoff`).
    pub error_json: bool,
    /// Content types this fetch also admits, within the page cap (`""`: no content type). The
    /// caller parses them: Pubky objects (`pubky::OBJECT_TYPES`), PKARR relay payloads
    /// (`pkarr_relays::PAYLOAD_TYPES`).
    pub extra_types: &'static [&'static str],
    pub validators: Validators,
    /// How long the request itself may take, once it has its host slot: connecting, sending and
    /// the whole answer, of each hop. The wait for the slot does not count (a request queued
    /// behind its host's pace is not a slow host). Past it the fetch fails as
    /// [`RequestDeadline`]. Without one, only the fetch's own ten seconds bound it.
    pub request_deadline: Option<Duration>,
    /// The longest this request waits for its host's slot, when shorter than `MAX_SLOT_WAIT`: a
    /// slot further away answers `RetryLater` (`rate_limited`, no request sent, no charge), as
    /// one past `MAX_SLOT_WAIT` always does.
    pub slot_wait: Option<Duration>,
    /// Pace this request by its host's slot alone, not also by its address's: for the
    /// operator's own endpoints (the PKARR relays), never for an address a link steers.
    pub host_slot_only: bool,
}
impl std::fmt::Debug for Fetched {
    /// The body's length, never the body: a token endpoint's answer holds the token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetched")
            .field("url", &self.url.as_str())
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("charset", &self.charset)
            .field("retry_after_secs", &self.retry_after_secs)
            .field("location", &self.location)
            .field("permanent_redirect", &self.permanent_redirect)
            .field("truncated", &self.truncated)
            .field("validators", &self.validators)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .finish()
    }
}
impl std::fmt::Debug for FetchOptions {
    /// A sensitive request's header and form values are its credentials: never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |values: &[(&'static str, String)]| -> Vec<(&'static str, String)> {
            values
                .iter()
                .map(|(k, v)| {
                    (
                        *k,
                        if self.sensitive {
                            "[REDACTED]".to_owned()
                        } else {
                            v.clone()
                        },
                    )
                })
                .collect()
        };
        f.debug_struct("FetchOptions")
            .field("headers", &redact(&self.headers))
            .field("json_body", &self.json_body)
            .field("form_body", &self.form_body.as_deref().map(redact))
            .field("cache_tag", &self.cache_tag.as_ref().map(|_| "[REDACTED]"))
            .field("uncached", &self.uncached)
            .field("sensitive", &self.sensitive)
            .field(
                "query_secret",
                &self.query_secret.as_ref().map(|(k, _)| (*k, "[REDACTED]")),
            )
            .field("accept", &self.accept)
            .field("user_agent", &self.user_agent)
            .field("feed_cap", &self.feed_cap)
            .field("feed_read", &self.feed_read)
            .field("body_cap", &self.body_cap)
            .field("feed_hint", &self.feed_hint)
            .field("head_cap", &self.head_cap)
            .field("error_json", &self.error_json)
            .field("extra_types", &self.extra_types)
            .field("validators", &self.validators)
            .field("request_deadline", &self.request_deadline)
            .field("slot_wait", &self.slot_wait)
            .finish()
    }
}
impl FetchOptions {
    /// A plain JSON GET: `Accept: application/json`, every other option at its default.
    pub fn json() -> Self {
        Self {
            accept: Some("application/json".into()),
            ..Default::default()
        }
    }
    /// A request has one body: JSON or a form, never both.
    fn one_body(&self) -> Result<()> {
        if self.json_body.is_some() && self.form_body.is_some() {
            return Err(refused("a request has one body: JSON or a form"));
        }
        if self.query_secret.is_some() && !self.sensitive {
            return Err(refused(
                "a credential in the query needs a sensitive request",
            ));
        }
        Ok(())
    }
}
/// The URL a request is sent to: `url` with the request's query credential
/// ([`FetchOptions::query_secret`]) appended. Only a transport calls it, for the request it sends.
pub fn wire_url(url: &Url, options: &FetchOptions) -> Url {
    let mut wire = url.clone();
    if let Some((name, value)) = &options.query_secret {
        wire.query_pairs_mut().append_pair(name, value);
    }
    wire
}
/// The wait a 429 without `Retry-After` stands for: the five minutes a failure is cached.
pub const UNNAMED_429_WAIT: u64 = 300;
/// A refusal by the egress policy, classed `policy`: not broken content, and not transient.
fn refused(message: &'static str) -> anyhow::Error {
    anyhow::Error::new(super::ErrorClass("policy")).context(message)
}
#[async_trait]
pub trait Transport: Send + Sync {
    async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched>;
    async fn get_with(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        _options: &FetchOptions,
    ) -> Result<Fetched> {
        self.get(url, addresses).await
    }
}
pub struct PinnedTransport;
#[async_trait]
impl Transport for PinnedTransport {
    async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
        self.get_with(url, addresses, &FetchOptions::default())
            .await
    }
    async fn get_with(
        &self,
        url: &Url,
        addresses: &[SocketAddr],
        options: &FetchOptions,
    ) -> Result<Fetched> {
        // A client per request: `resolve_to_addrs` pins this request's host to the addresses the
        // policy just checked, so the connection goes where the check looked and a second DNS
        // answer cannot move it. The price is no connection reuse (a new TCP and TLS handshake
        // each time), which the per-host pacing makes small.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(options.user_agent.as_deref().unwrap_or(USER_AGENT))
            .resolve_to_addrs(url.host_str().context("missing host")?, addresses)
            .build()?;
        // The answer is read as the URL's, never the wire's: a query credential stays here.
        let wire = wire_url(url, options);
        let mut request = if let Some(body) = &options.json_body {
            client.post(wire).json(body)
        } else if let Some(form) = &options.form_body {
            client.post(wire).form(form)
        } else {
            client.get(wire)
        }
        .header(
            "accept",
            options
                .accept
                .clone()
                .unwrap_or_else(|| default_accept(url)),
        );
        for (key, value) in &options.headers {
            request = request.header(*key, value);
        }
        if let Some(v) = &options.validators.etag {
            request = request.header("if-none-match", v);
        }
        if let Some(v) = &options.validators.last_modified {
            request = request.header("if-modified-since", v);
        }
        // A reqwest error names its URL, query and all (a calendar's token): never in an error.
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        read_response_with(url.clone(), response, options).await
    }
}
/// Whether a content type (lower case, without parameters) is an RSS, Atom or other XML feed's:
/// the bodies a read cuts at the page cap, or at `FetchOptions::feed_read`, rather than refuses.
pub fn xml_feed_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text/xml"
            | "application/xml"
            | "application/rss+xml"
            | "application/atom+xml"
            | "application/rdf+xml"
    )
}
/// The largest `limits.html_head_bytes`: no HTML body the egress returns is longer.
pub const MAX_HTML_HEAD_BYTES: usize = 16 * 1024 * 1024;

/// Whether a fetched page is within what a page parser takes: the page cap, a page cut there
/// (at most where a long head closes), or a whole HTML page within the head budget.
pub fn page_fits(f: &Fetched) -> bool {
    f.body.len() <= BODY_CAP
        || f.truncated
        || (matches!(
            f.content_type.as_str(),
            "text/html" | "application/xhtml+xml"
        ) && f.body.len() <= MAX_HTML_HEAD_BYTES)
}

pub async fn read_response_with(
    url: Url,
    mut response: reqwest::Response,
    options: &FetchOptions,
) -> Result<Fetched> {
    let header = |key: &str| {
        response
            .headers()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .filter(|s| s.len() <= 2048)
            .map(str::to_owned)
    };
    // Every `Link` header, as one comma-separated value (RFC 8288), up to 2 KB of whole headers.
    let mut link = String::new();
    for value in response.headers().get_all("link").iter() {
        let Ok(value) = value.to_str() else { continue };
        if link.len() + value.len() + 2 > 2048 {
            continue;
        }
        if !link.is_empty() {
            link.push_str(", ");
        }
        link.push_str(value);
    }
    let validators = Validators {
        link: Some(link).filter(|s| !s.is_empty()),
        etag: header("etag"),
        last_modified: header("last-modified"),
    };
    let status = response.status().as_u16();
    // A 429 that names no wait is held five minutes; other answers default to a minute. A spent
    // quota's reset (`X-RateLimit-Reset` beside `X-RateLimit-Remaining: 0`) names its wait.
    let retry_after_secs = super::osm::retry_after_named(response.headers())
        .or_else(|| {
            matches!(status, 403 | 429)
                .then(|| quota_reset(response.headers(), std::time::SystemTime::now()))
                .flatten()
        })
        .unwrap_or(if status == 429 { UNNAMED_429_WAIT } else { 60 });
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let charset = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(charset_parameter);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut body = vec![];
    let mut truncated = false;
    if (200..300).contains(&status) {
        let feed_type = matches!(
            content_type.as_str(),
            "text/calendar"
                | "application/ics"
                | "text/xml"
                | "application/xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/rdf+xml"
                | "application/feed+json"
        );
        let possible = feed_type
            || matches!(
                content_type.as_str(),
                "text/plain" | "application/octet-stream" | ""
            );
        let extra = options.extra_types.contains(&content_type.as_str());
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !possible && !extra {
            return Err(refused("content type is not allowed"));
        }
        let html = matches!(content_type.as_str(), "text/html" | "application/xhtml+xml");
        // An RSS or Atom feed read without a feed cap keeps its first megabyte like a page, or
        // as much as the read asks for (`feed_read`): its channel and newest items come first
        // (`cards::feed_prefix` completes the cut).
        let xml_feed = xml_feed_type(&content_type) && options.feed_cap.is_none();
        // A file whose card comes from its URL (an image, a PDF) is cut at the page cap, never
        // refused: its size says nothing against the card.
        let url_card = content_type.starts_with("image/") || content_type == "application/pdf";
        let (mut max, refuse) = match options.body_cap {
            Some(cap) => (cap, true),
            None if xml_feed => (
                options.feed_read.map_or(BODY_CAP, |n| n.max(BODY_CAP)),
                false,
            ),
            None if html || url_card => (BODY_CAP, false),
            None => (options.feed_cap.unwrap_or(BODY_CAP), true),
        };
        // A page past the page cap is read on to the head budget, whole or cut there: its head
        // may still be open, and only the parser can say where it ends (a `</head>` or `<body`
        // found by a look at the text may be text in a script). No check stops the read early.
        let head_cap = options
            .head_cap
            .filter(|cap| html && options.body_cap.is_none() && *cap > BODY_CAP);
        if refuse && response.content_length().is_some_and(|n| n > max as u64) {
            return Err(BodyTooLarge { cap: max }.into());
        }
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            if body.len().saturating_add(chunk.len()) > max {
                if refuse {
                    return Err(BodyTooLarge { cap: max }.into());
                }
                if let Some(cap) = head_cap.filter(|cap| max < *cap) {
                    max = cap;
                }
            }
            if body.len().saturating_add(chunk.len()) > max {
                body.extend_from_slice(&chunk[..max.saturating_sub(body.len())]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
            // A cut read stops as soon as it reaches its cap: a sender that stalls there costs no
            // timeout. At the page cap, the head budget applies.
            if !refuse && body.len() >= max {
                match head_cap {
                    Some(cap) if max < cap => max = cap,
                    _ => {
                        truncated = true;
                        break;
                    }
                }
            }
        }
        let sniff = String::from_utf8_lossy(&body[..body.len().min(256)])
            .trim_start_matches(['\u{feff}', ' ', '\r', '\n'])
            .starts_with("BEGIN:VCALENDAR");
        if !ALLOWED_TYPES.contains(&content_type.as_str()) && !feed_type && !sniff && !extra {
            return Err(refused("content type is not allowed"));
        }
    } else if (matches!(status, 403 | 429 | 503)
        && matches!(
            content_type.as_str(),
            "text/html" | "application/xhtml+xml" | "text/plain" | ""
        ))
        || (options.error_json
            && status >= 400
            && (content_type == "application/json" || content_type.ends_with("+json")))
    {
        // An error page keeps a bounded prefix of its body, so the interstitial classifier sees a
        // challenge (and an API its error); the status is preserved.
        const ERROR_BODY_CAP: usize = 64 * 1024;
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                // A connection reset inside the error page: its status is the answer, kept with
                // the prefix read so far.
                Err(error) if super::transport(&error) => {
                    truncated = true;
                    break;
                }
                Err(error) => return Err(error.without_url().into()),
            };
            let remaining = ERROR_BODY_CAP.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if chunk.len() >= remaining {
                truncated = true;
                break;
            }
        }
    }
    Ok(Fetched {
        permanent_redirect: false,
        truncated,
        validators,
        url,
        status,
        retry_after_secs,
        location,
        content_type,
        charset,
        body,
    })
}
type HostBudget = Arc<Mutex<Instant>>;
struct DiscoveryEntry {
    expires: Instant,
    accessed: Instant,
    value: Option<Fetched>,
    /// A failed read that was over its cap, replayed as such (a caller may fall back on it).
    too_large: Option<BodyTooLarge>,
}
type DiscoveryCache = Arc<Mutex<HashMap<String, DiscoveryEntry>>>;
tokio::task_local! {
    /// Set while the queue confirms a missing answer ([`confirming`]): the discovery keys this
    /// confirmation has already met.
    static CONFIRMING: std::cell::RefCell<HashSet<String>>;
}
/// Run `work` as the fetch that confirms a missing answer, which must be a fresh request. What
/// answer a resolver reads as "not there" is its own (a portal's 401, a 400 for an unknown ID, a
/// 200 with no rows), so the first time the confirmation meets a discovery key, the cache's
/// answer for it is asked again, not replayed, unless it names a wait ([`names_a_wait`]): that
/// is replayed, so a wait the upstream named is never cut short. Later reads of the key in the
/// same confirmation replay its new answer, so a batch asks a shared endpoint once.
pub async fn confirming<F: std::future::Future>(work: F) -> F::Output {
    CONFIRMING.scope(Default::default(), work).await
}
/// Whether a cached HTTP answer is a wait a resolver keeps: a 408, a 429 or a 5xx (each read as
/// `RetryLater` with its `Retry-After`), or a client error the caller names in
/// [`FetchOptions::waits`].
pub fn names_a_wait(status: u16, waits: &[u16]) -> bool {
    matches!(status, 408 | 429) || status >= 500 || waits.contains(&status)
}
#[derive(Clone)]
pub struct Egress {
    cfg: EgressConfig,
    network_relays: bool,
    dns: Arc<dyn Dns>,
    transport: Arc<dyn Transport>,
    hosts: Arc<Mutex<HashMap<String, HostBudget>>>,
    discovered: DiscoveryCache,
    discoveries: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}
impl Egress {
    /// One bounded Nostr subscription. DNS is checked once and the checked address is connected directly.
    pub async fn nostr_request(
        &self,
        uri: &str,
        filter: serde_json::Value,
        deadline_ms: u64,
    ) -> Result<Vec<serde_json::Value>> {
        let key = format!(
            "{uri}|REQ:{}",
            blake3::hash(&serde_json::to_vec(&filter)?).to_hex()
        );
        let fetched = self
            .cached(key, 3600, &[], &[], async {
                let values = self.nostr_request_inner(uri, filter, deadline_ms).await?;
                Ok(Fetched {
                    permanent_redirect: false,
                    truncated: false,
                    url: Url::parse(uri)?,
                    validators: Default::default(),
                    content_type: "application/json".into(),
                    charset: None,
                    status: 200,
                    retry_after_secs: 60,
                    location: None,
                    body: serde_json::to_vec(&values)?,
                })
            })
            .await?;
        Ok(tokio::task::spawn_blocking(move || serde_json::from_slice(&fetched.body)).await??)
    }
    async fn nostr_request_inner(
        &self,
        uri: &str,
        filter: serde_json::Value,
        deadline_ms: u64,
    ) -> Result<Vec<serde_json::Value>> {
        use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
        tokio::time::timeout(Duration::from_millis(deadline_ms), async {
            let url = Url::parse(uri)?;
            if url.scheme() != "wss" || !url.username().is_empty() || url.password().is_some() {
                bail!("invalid relay URL");
            }
            let host = url
                .host_str()
                .context("relay host missing")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if !self.allows_host(&host) {
                bail!("relay denied by egress policy");
            }
            let port = url.port_or_known_default().context("relay port")?;
            if BAD_PORTS.contains(&port) {
                bail!("relay port blocked by egress policy");
            }
            let addresses = self.dns.lookup(&host, port).await?;
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                bail!("relay DNS resolved to non-public address");
            }
            anyhow::ensure!(
                self.network_relays,
                "relay network disabled for injected transports"
            );
            let _permit = self.host_permit(&host).await?;
            let stream = tokio::net::TcpStream::connect(addresses.as_slice()).await?;
            let config = WebSocketConfig::default()
                .max_message_size(Some(BODY_CAP))
                .max_frame_size(Some(BODY_CAP));
            let mut observation = crate::telemetry::FetchGuard::start();
            let (mut socket, _) =
                tokio_tungstenite::client_async_tls_with_config(uri, stream, Some(config), None)
                    .await?;
            observation.status(101);
            nostr_subscription(&mut socket, filter).await
        })
        .await?
    }
    pub fn allows(&self, uri: &str) -> bool {
        Url::parse(uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .is_some_and(|host| self.allows_host(&host))
    }
    fn allows_host(&self, host: &str) -> bool {
        // Read as the operator's rules are (`canonical_host`), so a rule and a host written
        // alike always meet; what names no host is refused.
        let Some(host) = canonical_host(host) else {
            return false;
        };
        let host = host.to_string();
        !super::osm::osm_host(&host)
            && !self.cfg.deny_hosts.iter().any(|h| host_matches(&host, h))
            && !self.cfg.off_hosts.iter().any(|h| host_matches(&host, h))
            && (self.cfg.allow_hosts.is_empty()
                || self.cfg.allow_hosts.iter().any(|h| host_matches(&host, h)))
    }
    /// The operator's contact address (`http.contact`), if set.
    pub fn contact(&self) -> Option<&str> {
        Some(self.cfg.contact.as_str()).filter(|c| !c.is_empty())
    }
    pub fn new(cfg: EgressConfig) -> Self {
        let mut e = Self::with_backends(cfg, Arc::new(SystemDns), Arc::new(PinnedTransport));
        e.network_relays = true;
        e
    }
    pub fn with_backends(
        cfg: EgressConfig,
        dns: Arc<dyn Dns>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            cfg,
            network_relays: false,
            dns,
            transport,
            hosts: Arc::default(),
            discovered: Arc::default(),
            discoveries: Arc::default(),
        }
    }
    pub async fn fetch(&self, uri: &str) -> Result<Fetched> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.fetch_options_inner(uri, &FetchOptions::default()),
        )
        .await
        .context("web resolution exceeded 10 seconds")?
    }
    pub async fn fetch_with(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        options.one_body()?;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            self.fetch_options_inner(uri, options),
        )
        .await?;
        if options.sensitive {
            result
                .map(|mut fetched| {
                    // Parsers may retain the final URL as an alias. Credentials from
                    // configuration must never enter documents, logs, or change feeds.
                    fetched.url.set_query(None);
                    fetched.url.set_fragment(None);
                    fetched
                })
                .map_err(|error| {
                    // A reqwest cause can embed the complete URL, including API credentials.
                    if error.is::<super::interstitial::Blocked>() {
                        anyhow::Error::new(super::interstitial::Blocked)
                    } else if let Some(too_large) = error.downcast_ref::<BodyTooLarge>() {
                        anyhow::Error::new(*too_large)
                    } else if let Some(retry) = error.downcast_ref::<super::RetryLater>() {
                        anyhow::Error::new(super::RetryLater {
                            class: retry.class,
                            seconds: retry.seconds,
                        })
                    } else if super::unreached(&error) {
                        anyhow::Error::new(super::Unreached)
                    } else {
                        anyhow::Error::new(super::ErrorClass(super::error_class(&error)))
                    }
                })
        } else {
            result
        }
    }

    /// Concurrent references share a flight. Successful responses use the requested TTL;
    /// failures use five minutes, or the wait a `RetryLater` answer names when that is shorter.
    /// The LRU cache retains at most 10,000 entries / 32 MiB.
    /// Capacity eviction can shorten a TTL; host budgets still apply to every fetch.
    pub async fn discover(&self, uri: &str, options: &FetchOptions, ttl: u64) -> Result<Fetched> {
        self.cached(
            Self::discovery_key(uri, options)?,
            ttl,
            options.uncached,
            options.waits,
            self.fetch_with(uri, options),
        )
        .await
    }
    /// The cache key of a request: its URL and a hash of what else identifies it (its body, and
    /// its headers or else its `cache_tag`). A key holds the URL, which may carry credentials in
    /// its query: never log one.
    fn discovery_key(uri: &str, options: &FetchOptions) -> Result<String> {
        options.one_body()?;
        let mut url = Url::parse(uri)?;
        url.set_fragment(None);
        let mut key = url.to_string();
        if options.json_body.is_some()
            || options.form_body.is_some()
            || options.cache_tag.is_some()
            || !options.headers.is_empty()
        {
            let identity = match &options.cache_tag {
                Some(tag) => serde_json::to_vec(&(&options.json_body, &options.form_body, tag))?,
                None => {
                    serde_json::to_vec(&(&options.json_body, &options.form_body, &options.headers))?
                }
            };
            key.push_str(&format!("|request:{}", blake3::hash(&identity).to_hex()));
        }
        // A read with another cap gets another answer (a refusal is replayed only to its cap).
        if options.body_cap.is_some() || options.feed_cap.is_some() {
            key.push_str(&format!(
                "|cap:{}:{}",
                options.body_cap.unwrap_or(0),
                options.feed_cap.unwrap_or(0)
            ));
        }
        // A feed read further is another answer than one cut at the page cap.
        if let Some(n) = options.feed_read {
            key.push_str(&format!("|read:{n}"));
        }
        if options.keep_redirect {
            key.push_str("|redirect");
        }
        Ok(key)
    }
    /// Put an answer in the discovery cache as the answer to `uri` (when it holds none): a batched
    /// request's answer for one of the URLs it covered, so that URL's own lookup asks nothing.
    pub async fn seed(
        &self,
        uri: &str,
        options: &FetchOptions,
        ttl: u64,
        fetched: Fetched,
    ) -> Result<()> {
        let key = Self::discovery_key(uri, options)?;
        self.cached(
            key,
            ttl,
            &[],
            options.waits,
            std::future::ready(Ok(fetched)),
        )
        .await
        .map(|_| ())
    }
    /// Whether the discovery cache holds a live answer for `uri` (a batch may skip what its
    /// items already have).
    pub async fn discovered(&self, uri: &str, options: &FetchOptions) -> Result<bool> {
        let key = Self::discovery_key(uri, options)?;
        let now = Instant::now();
        Ok(self
            .discovered
            .lock()
            .await
            .get(&key)
            .is_some_and(|entry| entry.expires > now && entry.value.is_some()))
    }
    /// A successful HTTP response can still contain a transient application error.
    /// Shorten only its existing cache entry, without extending older expiration times.
    pub async fn shorten_discovery(
        &self,
        uri: &str,
        options: &FetchOptions,
        seconds: u64,
    ) -> Result<()> {
        let key = Self::discovery_key(uri, options)?;
        if let Some(entry) = self.discovered.lock().await.get_mut(&key) {
            entry.expires = entry
                .expires
                .min(Instant::now() + Duration::from_secs(seconds.clamp(1, 3600)));
        }
        Ok(())
    }
    /// Drop the discovery cache's answer for a request: an answer that must not stay in memory
    /// (a record the public can't read), whatever its TTL.
    pub async fn forget_discovery(&self, uri: &str, options: &FetchOptions) -> Result<()> {
        let key = Self::discovery_key(uri, options)?;
        self.discovered.lock().await.remove(&key);
        Ok(())
    }
    async fn cached(
        &self,
        key: String,
        ttl: u64,
        uncached: &[u16],
        waits: &[u16],
        fetch: impl std::future::Future<Output = Result<Fetched>>,
    ) -> Result<Fetched> {
        let lock = {
            let mut flights = self.discoveries.lock().await;
            flights.retain(|_, lock| Arc::strong_count(lock) > 1);
            flights.entry(key.clone()).or_default().clone()
        };
        let _guard = lock.lock().await;
        {
            let mut cache = self.discovered.lock().await;
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires > now);
            // A cached failure with no answer (a `RetryLater`, a transport error) is a wait too.
            let stale = CONFIRMING
                .try_with(|met| met.borrow_mut().insert(key.clone()))
                .unwrap_or(false)
                && cache.get(&key).is_some_and(|entry| match &entry.value {
                    Some(f) => !names_a_wait(f.status, waits),
                    None => entry.too_large.is_some(),
                });
            if stale {
                cache.remove(&key);
            }
            if let Some(entry) = cache.get_mut(&key) {
                entry.accessed = now;
                if let Some(too_large) = entry.too_large {
                    return Err(too_large.into());
                }
                return entry.value.clone().ok_or_else(|| {
                    super::RetryLater {
                        class: "discovery_cached",
                        seconds: entry
                            .expires
                            .saturating_duration_since(now)
                            .as_secs()
                            .max(1),
                    }
                    .into()
                });
            }
        }
        let result = fetch.await;
        // Our own host budget is not the page's answer: ask again when the slot comes.
        if result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<super::RetryLater>())
            .is_some_and(|r| r.class == "rate_limited")
        {
            return result;
        }
        // An answer the request says is not the URL's (an authentication failure) is never kept.
        if result.as_ref().is_ok_and(|f| uncached.contains(&f.status)) {
            return result;
        }
        let mut cache = self.discovered.lock().await;
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires > now);
        let retained = result
            .as_ref()
            .ok()
            .filter(|f| f.body.len() <= 32 * 1024 * 1024);
        let needed = retained.map_or(0, |f| f.body.len());
        let mut bytes: usize = cache
            .values()
            .filter_map(|entry| entry.value.as_ref())
            .map(|f| f.body.len())
            .sum();
        if cache.len() >= 10000 || bytes.saturating_add(needed) > 32 * 1024 * 1024 {
            let mut oldest: Vec<_> = cache
                .iter()
                .map(|(k, entry)| (entry.accessed, k.clone()))
                .collect();
            oldest.sort_unstable();
            for (_, key) in oldest {
                if cache.len() < 10000 && bytes.saturating_add(needed) <= 32 * 1024 * 1024 {
                    break;
                }
                if let Some(entry) = cache.remove(&key) {
                    bytes = bytes.saturating_sub(entry.value.as_ref().map_or(0, |f| f.body.len()));
                }
            }
        }
        let ttl = if retained.is_some_and(|f| (200..400).contains(&f.status)) {
            ttl.clamp(3600, 31536000)
        } else if let Some(asked) = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<super::RetryLater>())
        {
            // A server that said how long to wait is asked again then, within five minutes.
            asked.seconds.clamp(1, 300)
        } else {
            300
        };
        cache.insert(
            key,
            DiscoveryEntry {
                expires: now + Duration::from_secs(ttl),
                accessed: now,
                value: retained.cloned(),
                too_large: result
                    .as_ref()
                    .err()
                    .and_then(|e| e.chain().find_map(|c| c.downcast_ref::<BodyTooLarge>()))
                    .copied(),
            },
        );
        result
    }
    async fn fetch_options_inner(&self, uri: &str, options: &FetchOptions) -> Result<Fetched> {
        let requested = Url::parse(uri)?;
        let mut url = requested.clone();
        let mut permanent = true;
        // A redirect within the same host keeps the host's slot; a slow host's next slot is
        // further away than a fetch may wait, so a new one would always answer RetryLater. Each
        // request holds its host's slot and its first pinned address's (the address with its
        // slot), so many names on one server are paced together; a same-host hop to another
        // address takes that address's turn.
        type Slots = (
            String,
            OwnedMutexGuard<Instant>,
            Option<(String, OwnedMutexGuard<Instant>)>,
        );
        let mut permit: Option<Slots> = None;
        for redirects in 0..=3 {
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(refused(
                    "only public HTTP(S) URLs without credentials are allowed",
                ));
            }
            let host = url
                .host_str()
                .context("missing host")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if !self.allows_host(&host) {
                return Err(anyhow::Error::new(super::ErrorClass("policy"))
                    .context("host denied by operator/OSM policy"));
            }
            // Amazon is never fetched, also where another link redirects to it.
            if crate::kinds::products::on_amazon(&url) {
                return Err(refused("Amazon is never fetched"));
            }
            let port = url.port_or_known_default().context("missing port")?;
            if BAD_PORTS.contains(&port) {
                return Err(refused("port blocked by egress policy"));
            }
            let addresses = match url.host().context("missing host")? {
                Host::Ipv4(v) => vec![SocketAddr::new(IpAddr::V4(v), port)],
                Host::Ipv6(v) => vec![SocketAddr::new(IpAddr::V6(v), port)],
                Host::Domain(_) => self
                    .dns
                    .lookup(&host, port)
                    .await
                    .context(super::ErrorClass("dns"))?,
            };
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                return Err(anyhow::Error::new(super::ErrorClass("policy"))
                    .context("egress refused non-public address"));
            }
            // Pin checked addresses to the connection; a second DNS lookup could rebind to a private IP.
            // A service's own host, which no link can multiply, is paced by its name alone.
            let address = (!options.host_slot_only && !self.service_host(&host))
                .then(|| address_slot(addresses[0].ip()));
            match &mut permit {
                Some((held, guard, at_address)) if *held == host => {
                    **guard = Instant::now() + self.host_interval(&host);
                    match (at_address.as_mut(), &address) {
                        (Some((held_address, at)), Some(address)) if held_address == address => {
                            **at = Instant::now() + self.address_interval(address, &host);
                        }
                        (_, Some(address)) => {
                            // A new DNS answer: the other address's slot goes before this
                            // one's turn is waited for.
                            *at_address = None;
                            let wait = options
                                .slot_wait
                                .map_or(MAX_SLOT_WAIT, |w| w.min(MAX_SLOT_WAIT));
                            let at = self.address_permit_within(address, &host, wait).await?;
                            *at_address = Some((address.clone(), at));
                        }
                        (_, None) => *at_address = None,
                    }
                }
                _ => {
                    // Release the other host's slots before waiting for this one's.
                    drop(permit.take());
                    let wait = options
                        .slot_wait
                        .map_or(MAX_SLOT_WAIT, |w| w.min(MAX_SLOT_WAIT));
                    let deadline = Instant::now() + wait;
                    let mut guard = self.host_permit_within(&host, wait).await?;
                    let at_address = match &address {
                        Some(address) => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            match self.address_permit_within(address, &host, left).await {
                                Ok(at_address) => Some((address.clone(), at_address)),
                                Err(error) => {
                                    // No request is sent: the host's turn is not used up.
                                    *guard = Instant::now();
                                    return Err(error);
                                }
                            }
                        }
                        None => None,
                    };
                    permit = Some((host.clone(), guard, at_address));
                }
            }
            let mut observation = crate::telemetry::FetchGuard::start();
            let with_agent = FetchOptions {
                user_agent: Some(user_agent(&self.cfg.contact)),
                head_cap: options.head_cap.or(Some(self.cfg.html_head_bytes)),
                ..options.clone()
            };
            let options = &with_agent;
            // The slot is taken: a caller's deadline for the request starts here.
            let sent = self.transport.get_with(&url, &addresses, options);
            let mut response = match options.request_deadline {
                Some(deadline) => tokio::time::timeout(deadline, sent)
                    .await
                    .map_err(|_| RequestDeadline(deadline))??,
                None => sent.await?,
            };
            observation.status(response.status);
            drop(observation);
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                if options.keep_redirect {
                    return Ok(response);
                }
                if options.sensitive
                    || options.json_body.is_some()
                    || options.form_body.is_some()
                    || !options.headers.is_empty()
                {
                    return Err(refused("redirect of authenticated/API request refused"));
                }
                if redirects == 3 {
                    return Err(refused("more than 3 redirects"));
                }
                permanent &= matches!(response.status, 301 | 308);
                url = url.join(
                    response
                        .location
                        .as_deref()
                        .context("redirect omitted location")?,
                )?;
                continue;
            }
            // The wall check: an answer its addresses, status and type settle (an API's JSON)
            // takes no parse slot. Any other is parsed as HTML in one, moved there and back.
            let page = match super::interstitial::unparsed(&requested, &response) {
                Some(page) => page,
                None => {
                    let original = requested.clone();
                    // Boxed: the job holds the answer, kept off callers' debug stacks.
                    let (page, answer) = Box::pin(super::html::blocking(move || {
                        (super::interstitial::inspect(&original, &response), response)
                    }))
                    .await?;
                    response = answer;
                    page?
                }
            };
            if page == super::interstitial::Page::Blocked {
                crate::telemetry::set_outcome(crate::telemetry::last_fetch(), "blocked");
                return Err(super::interstitial::Blocked.into());
            }
            if let super::interstitial::Page::Redirect(target) = page {
                if options.keep_redirect {
                    response.location = Some(url.join(&target)?.to_string());
                    return Ok(response);
                }
                if options.sensitive
                    || options.json_body.is_some()
                    || options.form_body.is_some()
                    || !options.headers.is_empty()
                {
                    return Err(refused("redirect of authenticated/API request refused"));
                }
                if redirects == 3 {
                    return Err(refused("more than 3 redirects"));
                }
                permanent = false;
                url = url.join(&target)?;
                continue;
            }
            if response.status == 429 && !options.error_json {
                return Err(super::RetryLater {
                    class: "http_4xx",
                    seconds: response.retry_after_secs,
                }
                .into());
            }
            response.permanent_redirect = redirects > 0 && permanent;
            return Ok(response);
        }
        unreachable!()
    }
    /// The host's next slot, held for the whole request (and its redirects within the host): one
    /// request per host at a time. An
    /// operator's `host_rates` entry sets the rate; else a source's published rate lowers the
    /// default, never raises it. A slot further away than `MAX_SLOT_WAIT`, or a queue longer than
    /// that, answers `RetryLater` instead of a wait the fetch timeout would cut short. Only a
    /// request that gets its slot moves the next one, so a cancelled waiter holds nothing back.
    async fn host_permit(&self, host: &str) -> Result<OwnedMutexGuard<Instant>> {
        self.host_permit_within(host, MAX_SLOT_WAIT).await
    }
    /// [`host_permit`](Self::host_permit), waiting at most `wait` (never more than
    /// `MAX_SLOT_WAIT`) for the slot.
    async fn host_permit_within(
        &self,
        host: &str,
        wait: Duration,
    ) -> Result<OwnedMutexGuard<Instant>> {
        self.slot_permit_within(host, self.host_interval(host), wait)
            .await
    }
    /// The turn of `address` (its [`address_slot`]) for a request to `host`, waiting at most
    /// `wait` as [`host_permit_within`](Self::host_permit_within) does.
    async fn address_permit_within(
        &self,
        address: &str,
        host: &str,
        wait: Duration,
    ) -> Result<OwnedMutexGuard<Instant>> {
        self.slot_permit_within(address, self.address_interval(address, host), wait)
            .await
    }
    /// Whether `host` is a service's own, named in the code with its pace: a source's or an
    /// also-on source's (`budgets`, in `source_rates`) or a published one (`PUBLISHED_RATES`, an
    /// exact rule: a `*.` rule's names are as many as links make). The fixed API hosts sources
    /// call are one name each, so they take their host's slot only, as the PKARR relays do
    /// (`FetchOptions::host_slot_only`). Every other host (a link's, a redirect's, a discovered
    /// endpoint's) is paced by its address too.
    fn service_host(&self, host: &str) -> bool {
        self.cfg.source_rates.contains_key(host)
            || PUBLISHED_RATES.iter().any(|(rule, _)| *rule == host)
    }
    /// The least time between two requests to one address: the default pace, or the host's when
    /// the operator set it faster. A slow host's own pace never slows the other names on its
    /// address.
    fn address_interval(&self, address: &str, host: &str) -> Duration {
        self.host_interval(address).min(self.host_interval(host))
    }
    /// The next turn of the slot named `slot` (a host, or an address: [`address_slot`]), the next
    /// one `interval` after it, waiting at most `wait`.
    async fn slot_permit_within(
        &self,
        slot: &str,
        interval: Duration,
        wait: Duration,
    ) -> Result<OwnedMutexGuard<Instant>> {
        let budget = {
            let mut hosts = self.hosts.lock().await;
            if hosts.len() >= 10000 {
                hosts.retain(|_, v| {
                    Arc::strong_count(v) > 1 || v.try_lock().map_or(true, |t| *t > Instant::now())
                });
            }
            hosts
                .entry(slot.into())
                .or_insert_with(|| Arc::new(Mutex::new(Instant::now())))
                .clone()
        };
        let retry = |wait: Duration| {
            anyhow::Error::new(super::RetryLater {
                class: "rate_limited",
                seconds: wait.as_secs() + 1,
            })
        };
        let deadline = Instant::now() + wait;
        let Ok(mut guard) = tokio::time::timeout_at(deadline, budget.lock_owned()).await else {
            return Err(retry(interval.max(wait)));
        };
        if *guard > deadline {
            return Err(retry(guard.saturating_duration_since(Instant::now())));
        }
        tokio::time::sleep_until(*guard).await;
        *guard = Instant::now() + interval;
        Ok(guard)
    }
}
/// The pacing slot of an address, apart from every host name's.
fn address_slot(ip: IpAddr) -> String {
    format!("address {ip}")
}
impl Egress {
    /// The least time between two requests to a host: its rate plus a stable per-host jitter,
    /// which avoids synchronized requests without weakening the minimum interval.
    fn host_interval(&self, host: &str) -> Duration {
        let jitter = f64::from(blake3::hash(host.as_bytes()).as_bytes()[0]) / 2550.0;
        Duration::from_secs_f64(1.0 / self.host_rate(host) + jitter)
    }
    fn host_rate(&self, host: &str) -> f64 {
        let default = self.cfg.host_requests_per_second;
        // A source's limit (`source_rates`) or a service's published one (`PUBLISHED_RATES`)
        // lowers the default, never raises it.
        match self.cfg.host_rates.get(host) {
            Some(rate) => *rate,
            None => self
                .cfg
                .source_rates
                .get(host)
                .copied()
                .into_iter()
                .chain(published_rate(host))
                .fold(default, f64::min),
        }
    }
}
/// Requests per second the services several sources or the resolver read publish as their limit,
/// or a cautious pace where none is published (one request at a time per host is the rule for
/// every host); it lowers a host's default pace, beside the sources' own limits
/// (`sources::budgets`, `also_on::budgets`: a service only one source reads).
/// A rule `*.example.org` covers the subdomains.
pub const PUBLISHED_RATES: &[(&str, f64)] = &[
    // MusicBrainz: 1 request/s per IP, 503 above that.
    ("musicbrainz.org", 1.0),
    // The Cover Art Archive (MusicBrainz's) publishes no limit: MusicBrainz's pace. Its covers'
    // redirects to archive.org are kept, never followed (`hubs::cover_art`).
    ("coverartarchive.org", 1.0),
    // Wikimedia: 200 requests/min with a User-Agent naming a contact (at most 3 at once).
    ("www.wikidata.org", 200.0 / 60.0),
    ("*.wikipedia.org", 200.0 / 60.0),
    ("commons.wikimedia.org", 200.0 / 60.0),
    // Open Library: 3 requests/s with a User-Agent and a contact.
    ("openlibrary.org", 3.0),
    // iTunes Search and Lookup: about 20 requests/min.
    ("itunes.apple.com", 20.0 / 60.0),
    // TVmaze: at least 20 calls every 10 seconds per IP (the films and media_records sources).
    ("api.tvmaze.com", 2.0),
    // The Wayback Machine's CDX API asks for a gentle pace (the whole host: tagged captures too).
    ("web.archive.org", 0.5),
    // The Nostr indexers and the Einundzwanzig portal publish no limit: a cautious pace.
    ("api.nostr.wine", 1.0),
    ("api.nostrarchives.com", 2.0),
    ("portal.einundzwanzig.space", 1.0),
];
/// The published rate of `host`, if it has one (the most specific rule).
pub fn published_rate(host: &str) -> Option<f64> {
    PUBLISHED_RATES
        .iter()
        .filter(|(rule, _)| host_matches(host, rule))
        .max_by_key(|(rule, _)| rule.len())
        .map(|(_, rate)| *rate)
}
/// A host as the egress policy compares it, the operator's rules (`config`) and the hosts it
/// asks alike: lower case and IDNA ASCII (`faß.de` is `xn--fa-hia.de`), without trailing dots
/// (`.`, and the full stops IDNA maps to one: `。`, `．`, `｡`, `%2e`), an IP address in its
/// canonical form. What is left once the dots go is parsed again, until nothing changes: without
/// them a name may be an address (`8.8.8..` is `8.8.0.8`, as a URL reads `8.8.8`). `None` for
/// what names no host. Idempotent.
pub fn canonical_host(name: &str) -> Option<url::Host> {
    let mut host = url::Host::parse(name.trim()).ok()?;
    while let url::Host::Domain(d) = &host {
        let trimmed = d.trim_end_matches('.');
        if trimmed.len() == d.len() {
            break;
        }
        host = url::Host::parse(trimmed).ok()?;
    }
    match host {
        url::Host::Domain(d) if d.is_empty() => None,
        host => Some(host),
    }
}
fn host_matches(host: &str, rule: &str) -> bool {
    let rule = rule.to_ascii_lowercase();
    if let Some(suffix) = rule.strip_prefix("*.") {
        host == suffix || host.strip_suffix(suffix).is_some_and(|s| s.ends_with('.'))
    } else {
        host == rule.trim_end_matches('.')
    }
}

async fn nostr_subscription<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    filter: serde_json::Value,
) -> Result<Vec<serde_json::Value>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    socket
        .send(Message::Text(
            serde_json::json!(["REQ", "pinakes", filter])
                .to_string()
                .into(),
        ))
        .await?;
    let mut events = vec![];
    let mut bytes = 0;
    let mut complete = false;
    for _ in 0..256 {
        let Some(message) = socket.next().await else {
            break;
        };
        let message = message?;
        bytes += message.len();
        if bytes > BODY_CAP {
            bail!("relay response exceeds cap");
        }
        if let Message::Text(text) = message {
            let value: serde_json::Value =
                tokio::task::spawn_blocking(move || serde_json::from_str(&text)).await??;
            if value[1] != "pinakes" {
                continue;
            }
            if value[0] == "EOSE" {
                complete = true;
                break;
            }
            if value[0] == "EVENT" {
                events.push(value[2].clone());
            }
            if value[0] == "CLOSED" {
                break;
            }
        }
    }
    socket
        .send(Message::Text("[\"CLOSE\",\"pinakes\"]".into()))
        .await?;
    let _ = socket.close(None).await;
    if !complete {
        bail!("relay did not finish subscription");
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    /// No test outside this module builds an egress on the system resolver (`Egress::new`,
    /// `SystemDns`), so `scripts/test` looks no name up and connects nowhere past loopback: a
    /// test answers from a fixed resolver and recorded answers (`sources::fixture`,
    /// `tests/common`). This module's own tests use `Egress::new` without fetching.
    #[test]
    fn no_test_elsewhere_reaches_the_network() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = vec![];
        rust_files(&crates, &mut files);
        let mut found = vec![];
        for path in files {
            let rel = path
                .strip_prefix(&crates)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "pinakes-core/src/resolver/egress.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // A test file whole; a source file from its first test module on.
            let tests = if rel.contains("/tests/") {
                Some(text.as_str())
            } else if rel.contains("/src/") {
                text.find("#[cfg(test)]").map(|at| &text[at..])
            } else {
                None
            };
            for line in tests.into_iter().flat_map(str::lines) {
                if line.contains("Egress::new(") || line.contains("SystemDns") {
                    found.push(format!("{rel}: {}", line.trim()));
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }
    #[tokio::test(start_paused = true)]
    async fn slow_hosts_answer_retry_later_instead_of_a_doomed_wait() {
        struct Ok200;
        #[async_trait]
        impl Transport for Ok200 {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "application/json".into(),
                    b"{}".to_vec(),
                ))
            }
        }
        let egress = Arc::new(Egress::with_backends(
            EgressConfig {
                source_rates: [
                    ("slow.example".to_owned(), 5.0 / 60.0),
                    ("fast.example".to_owned(), 50.0),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
            Arc::new(FakeDns),
            Arc::new(Ok200),
        ));
        // Three links of a host with a 12-second interval claimed together: one goes, the others
        // learn when to come back, at once.
        let tasks: Vec<_> = (0..3)
            .map(|n| {
                let egress = egress.clone();
                tokio::spawn(
                    async move { egress.fetch(&format!("https://slow.example/{n}")).await },
                )
            })
            .collect();
        let mut results = vec![];
        for task in tasks {
            results.push(task.await.unwrap());
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        for error in results.into_iter().filter_map(Result::err) {
            let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
            assert_eq!(retry.class, "rate_limited");
            assert!((9..=14).contains(&retry.seconds), "{}", retry.seconds);
        }
        tokio::time::advance(Duration::from_secs(13)).await;
        assert!(egress.fetch("https://slow.example/again").await.is_ok());
        // A source's rate never raises the default of one request per second.
        let start = Instant::now();
        egress.fetch("https://fast.example/1").await.unwrap();
        egress.fetch("https://fast.example/2").await.unwrap();
        assert!(Instant::now() - start >= Duration::from_secs(1));
    }
    #[tokio::test(start_paused = true)]
    async fn a_queue_behind_a_slow_request_answers_retry_later() {
        struct Slow;
        #[async_trait]
        impl Transport for Slow {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                if url.path() == "/slow" {
                    tokio::time::sleep(Duration::from_secs(7)).await;
                }
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "application/json".into(),
                    b"{}".to_vec(),
                ))
            }
        }
        let egress = Arc::new(Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            Arc::new(Slow),
        ));
        let slow = {
            let egress = egress.clone();
            tokio::spawn(async move { egress.fetch("https://host.example/slow").await })
        };
        tokio::task::yield_now().await;
        // A waiter that gives up holds nothing back.
        let cancelled = {
            let egress = egress.clone();
            tokio::spawn(async move { egress.fetch("https://host.example/cancelled").await })
        };
        tokio::task::yield_now().await;
        cancelled.abort();
        // The next one gives up before the fetch timeout, with a time to come back.
        let error = egress
            .fetch("https://host.example/queued")
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!((retry.class, retry.seconds), ("rate_limited", 6));
        assert!(slow.await.unwrap().is_ok());
        let start = Instant::now();
        assert!(egress.fetch("https://host.example/after").await.is_ok());
        assert!(Instant::now() - start <= Duration::from_secs(2));
    }
    #[tokio::test(start_paused = true)]
    async fn a_redirect_within_a_slow_host_keeps_its_slot() {
        struct Redirects;
        #[async_trait]
        impl Transport for Redirects {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                let redirect = url.path() == "/old";
                Ok(Fetched {
                    location: redirect.then(|| "/new".to_owned()),
                    ..Fetched::answer(
                        url.clone(),
                        if redirect { 301 } else { 200 },
                        "application/json".into(),
                        b"{}".to_vec(),
                    )
                })
            }
        }
        let egress = Egress::with_backends(
            EgressConfig {
                source_rates: [("slow.example".to_owned(), 5.0 / 60.0)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            Arc::new(FakeDns),
            Arc::new(Redirects),
        );
        // With a 12-second interval a second slot is always too far away: the hop keeps the first.
        let fetched = egress.fetch("https://slow.example/old").await.unwrap();
        assert_eq!(fetched.url.path(), "/new");
        assert!(fetched.permanent_redirect);
        // The next request still waits a full interval after the last hop.
        let error = egress
            .fetch("https://slow.example/other")
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!(retry.class, "rate_limited");

        // Our own budget is not the page's answer: discovery asks again once the slot comes.
        let options = FetchOptions::default();
        let error = egress
            .discover("https://slow.example/page", &options, 3600)
            .await
            .unwrap_err();
        let retry = error.downcast_ref::<crate::resolver::RetryLater>().unwrap();
        assert_eq!(retry.class, "rate_limited");
        tokio::time::advance(Duration::from_secs(13)).await;
        assert!(egress
            .discover("https://slow.example/page", &options, 3600)
            .await
            .is_ok());

        // A lookup whose redirect is the answer keeps it, and caches it apart from a followed one.
        let keep = FetchOptions {
            keep_redirect: true,
            ..Default::default()
        };
        tokio::time::advance(Duration::from_secs(13)).await;
        let kept = egress
            .discover("https://slow.example/old", &keep, 3600)
            .await
            .unwrap();
        assert_eq!(
            (kept.status, kept.url.path(), kept.location.as_deref()),
            (301, "/old", Some("/new"))
        );
        tokio::time::advance(Duration::from_secs(13)).await;
        let followed = egress
            .discover("https://slow.example/old", &options, 3600)
            .await
            .unwrap();
        assert_eq!((followed.status, followed.url.path()), (200, "/new"));
    }
    #[tokio::test]
    async fn body_cap_refuses_any_type_html_too() {
        use axum::{body::Body, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let html = |n: usize| ([("content-type", "text/html")], "x".repeat(n));
        let app = Router::new()
            .route("/small", get(move || async move { html(8 * 1024) }))
            .route("/large", get(move || async move { html(8 * 1024 + 1) }))
            .route(
                "/chunked",
                get(|| async {
                    let chunks = (0..3).map(|_| Ok::<_, std::io::Error>("x".repeat(4096)));
                    (
                        [("content-type", "text/html")],
                        Body::from_stream(futures_util::stream::iter(chunks)),
                    )
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let options = FetchOptions {
            body_cap: Some(8 * 1024),
            ..Default::default()
        };
        let fetch = |path: &str| {
            let url: Url = format!("http://{addr}{path}").parse().unwrap();
            let request = client.get(url.clone());
            let options = &options;
            async move { read_response_with(url, request.send().await.unwrap(), options).await }
        };
        assert_eq!(fetch("/small").await.unwrap().body.len(), 8 * 1024);
        for path in ["/large", "/chunked"] {
            let error = fetch(path).await.unwrap_err();
            assert!(
                error.downcast_ref::<BodyTooLarge>().is_some(),
                "{path}: {error:#}"
            );
        }
        server.abort();
    }
    /// Every request carries Pinakes's own User-Agent, with the operator's contact when one is
    /// set; a caller's value never reaches the wire.
    #[tokio::test]
    async fn every_request_carries_the_own_agent_with_the_operator_contact() {
        #[derive(Default)]
        struct Agents(std::sync::Mutex<Vec<Option<String>>>);
        #[async_trait]
        impl Transport for Agents {
            async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &Url,
                _: &[SocketAddr],
                options: &FetchOptions,
            ) -> Result<Fetched> {
                self.0.lock().unwrap().push(options.user_agent.clone());
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "application/json".into(),
                    b"{}".to_vec(),
                ))
            }
        }
        let with_contact = format!(
            "pinakes/{} (+https://pubky.app; info@synonym.to)",
            env!("CARGO_PKG_VERSION")
        );
        for (contact, agent, sent) in [
            ("info@synonym.to", None, with_contact.as_str()),
            ("info@synonym.to", Some(USER_AGENT), with_contact.as_str()),
            (
                "info@synonym.to",
                Some("Browser/1.0"),
                with_contact.as_str(),
            ),
            ("", None, USER_AGENT),
            ("", Some("Browser/1.0"), USER_AGENT),
        ] {
            let transport = Arc::new(Agents::default());
            let egress = Egress::with_backends(
                EgressConfig {
                    contact: contact.into(),
                    ..Default::default()
                },
                Arc::new(FakeDns),
                transport.clone(),
            );
            assert_eq!(egress.contact(), Some(contact).filter(|c| !c.is_empty()));
            let options = FetchOptions {
                user_agent: agent.map(str::to_owned),
                ..Default::default()
            };
            egress
                .fetch_with("https://public.example/api", &options)
                .await
                .unwrap();
            assert_eq!(transport.0.lock().unwrap()[0].as_deref(), Some(sent));
        }
    }
    /// An HTML page past the page cap is read on to the head budget, whole when it ends before
    /// it, cut there otherwise, whether or not its head closed by then: nothing in the text
    /// stops the read early (only the parser can say where a head ends). Without a head budget
    /// the page is cut at the page cap. A page parser takes what is read (`page_fits`).
    #[tokio::test]
    async fn a_page_past_the_cap_is_read_to_the_head_budget() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let html = |body: String| ([("content-type", "text/html; charset=utf-8")], body);
        let app = Router::new()
            .route(
                "/long-head",
                get(move || async move {
                    html(format!(
                        "<html><head><style>{}</style><title>Long head</title></HEAD><body>{}</body></html>",
                        "x".repeat(BODY_CAP + 50_000),
                        "y".repeat(500_000)
                    ))
                }),
            )
            .route(
                "/short-head",
                get(move || async move {
                    html(format!(
                        "<html><head><title>Short</title></head><body>{}</body></html>",
                        "y".repeat(3 * BODY_CAP)
                    ))
                }),
            )
            .route(
                "/implicit",
                get(move || async move {
                    html(format!(
                        "<html><head><title>Implicit</title><style>{}</style><BODY class=x><p>y</p></BODY></html>",
                        "x".repeat(BODY_CAP)
                    ))
                }),
            )
            .route(
                "/open-head",
                get(move || async move {
                    html(format!("<html><head><style>{}", "x".repeat(3 * BODY_CAP)))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let read = |path: &str, head_cap: Option<usize>| {
            let url: Url = format!("http://{addr}{path}").parse().unwrap();
            let request = client.get(url.clone());
            let options = FetchOptions {
                head_cap,
                ..Default::default()
            };
            async move { read_response_with(url, request.send().await.unwrap(), &options).await }
        };
        let head_cap = Some(2 * BODY_CAP);
        // 1.55 MB, within the budget: read whole, the title and the body with it.
        let long = read("/long-head", head_cap).await.unwrap();
        let text = String::from_utf8_lossy(&long.body);
        assert!(!long.truncated);
        assert!(text.contains("<title>Long head</title>"));
        assert!(text.ends_with("</html>"), "{}", &text[text.len() - 40..]);
        assert!(page_fits(&long));
        // Without a head budget, the page is cut at the page cap and the title is lost.
        let cut = read("/long-head", None).await.unwrap();
        assert_eq!(cut.body.len(), BODY_CAP);
        assert!(!String::from_utf8_lossy(&cut.body).contains("<title>"));
        // 3 MB with a short head: still read to the budget, and cut there.
        let short = read("/short-head", head_cap).await.unwrap();
        assert!(short.truncated);
        assert_eq!(short.body.len(), 2 * BODY_CAP);
        assert!(page_fits(&short));
        // A head the body opens implicitly, past the page cap: whole.
        let implicit = read("/implicit", head_cap).await.unwrap();
        assert!(!implicit.truncated);
        assert!(String::from_utf8_lossy(&implicit.body).ends_with("</html>"));
        assert!(page_fits(&implicit));
        // A head that never ends: cut at the budget.
        let open = read("/open-head", head_cap).await.unwrap();
        assert!(open.truncated);
        assert_eq!(open.body.len(), 2 * BODY_CAP);
        assert!(page_fits(&open));
        server.abort();
    }
    /// Nothing in a page's text ends its read early: a head of about 1.05 MB with `<body` or
    /// `</script>` in a script, a style or a comment (in every variant, including a script whose
    /// `<!--<script>` escapes the `</script>` inside it) is read whole, the title and meta tags
    /// after the decoy with it.
    #[tokio::test]
    async fn no_text_in_a_long_head_ends_its_read() {
        use axum::{extract::Path, routing::get, Router};
        fn page(decoy: &str) -> String {
            format!(
                "<html><head><style>{}</style>{decoy}<title>After</title><meta property='og:title' content='After'></head><body>x</body></html>",
                "x".repeat(BODY_CAP)
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/{kind}",
            get(|Path(kind): Path<String>| async move {
                let decoy = match kind.as_str() {
                    "script" => "<script>var tpl = '<body class=x>';</script>",
                    "style" => "<style>/* <body class=x> */ a { b: c }</style>",
                    "escaped" => "<script><!--<script></script><body class=x>--> </script>",
                    _ => "<!-- <body class=x> -->",
                };
                ([("content-type", "text/html")], page(decoy))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        for kind in ["script", "style", "comment", "escaped"] {
            let url: Url = format!("http://{addr}/{kind}").parse().unwrap();
            let options = FetchOptions {
                head_cap: Some(2 * BODY_CAP),
                ..Default::default()
            };
            let f =
                read_response_with(url.clone(), client.get(url).send().await.unwrap(), &options)
                    .await
                    .unwrap();
            let text = String::from_utf8_lossy(&f.body);
            assert!(!f.truncated, "{kind}");
            assert!(text.contains("<title>After</title>"), "{kind}");
            assert!(text.contains("og:title"), "{kind}");
            assert!(
                text.ends_with("</html>"),
                "{kind}: {}",
                &text[text.len() - 30..]
            );
        }
        server.abort();
    }
    /// A cut read stops as soon as it reaches its cap: a PDF that announces 3 MiB, sends exactly
    /// the page cap and stalls gets its bytes at once, as does a head that reaches the head budget
    /// exactly, instead of waiting out the fetch timeout.
    #[tokio::test]
    async fn a_read_stops_at_its_cap_without_another_read() {
        use axum::{body::Body, http::header, response::Response, routing::get, Router};
        fn stalled(content_type: &'static str, first: Vec<u8>) -> Response {
            let stream = futures_util::StreamExt::chain(
                futures_util::stream::iter([Ok::<_, std::io::Error>(first)]),
                futures_util::stream::pending(),
            );
            Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, 3 * BODY_CAP)
                .body(Body::from_stream(stream))
                .unwrap()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/paper.pdf",
                get(|| async { stalled("application/pdf", vec![b'%'; BODY_CAP]) }),
            )
            .route(
                "/open-head",
                get(|| async {
                    let mut page = b"<html><head><style>".to_vec();
                    page.resize(2 * BODY_CAP, b'x');
                    stalled("text/html", page)
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let read = |path: &str| {
            let url: Url = format!("http://{addr}{path}").parse().unwrap();
            let request = client.get(url.clone());
            let options = FetchOptions {
                head_cap: Some(2 * BODY_CAP),
                ..Default::default()
            };
            async move {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    read_response_with(url, request.send().await.unwrap(), &options),
                )
                .await
            }
        };
        let pdf = read("/paper.pdf")
            .await
            .expect("no wait past the page cap")
            .unwrap();
        assert!(pdf.truncated);
        assert_eq!(pdf.body.len(), BODY_CAP);
        let head = read("/open-head")
            .await
            .expect("no wait past the head budget")
            .unwrap();
        assert!(head.truncated);
        assert_eq!(head.body.len(), 2 * BODY_CAP);
        server.abort();
    }
    /// A failed request's error never names its URL: a calendar's token in the query stays out of
    /// every error and log. The error keeps its kind.
    #[tokio::test]
    async fn a_failed_request_does_not_name_its_url() {
        // A closed port: the connection is refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url: Url = format!("http://127.0.0.1:{}/calendar.ics?token=CANARY", addr.port())
            .parse()
            .unwrap();
        let error = PinnedTransport
            .get_with(&url, &[addr], &FetchOptions::default())
            .await
            .unwrap_err();
        let text = format!("{error:#} {error:?}");
        assert!(!text.contains("CANARY"), "{text}");
        assert_eq!(super::super::error_class(&error), "connect");
        // A refused connection never reached the upstream: the queue never dead-letters on it.
        assert!(super::super::unreached(&error));
    }
    /// A connection the peer resets, before the answer or inside its body, is a `connect`
    /// failure that never reached the upstream's answer: the queue never dead-letters on it.
    #[tokio::test]
    async fn a_reset_connection_is_unreached() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for partial in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let _ = socket.read(&mut request).await;
                if partial {
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: 100000\r\n\r\n<html><head>")
                        .await
                        .unwrap();
                    socket.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                // SO_LINGER 0: closing sends a reset, not a FIN.
                socket.set_zero_linger().unwrap();
                drop(socket);
            });
            let url: Url = format!("http://127.0.0.1:{}/page", addr.port())
                .parse()
                .unwrap();
            let error = PinnedTransport
                .get_with(&url, &[addr], &FetchOptions::default())
                .await
                .unwrap_err();
            assert_eq!(
                super::super::error_class(&error),
                "connect",
                "{partial} {error:#}"
            );
            assert!(super::super::unreached(&error), "{partial} {error:#}");
            server.await.unwrap();
        }
        // A reset inside an error page's body: the status was answered, and is kept.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/html\r\ncontent-length: 100000\r\n\r\n<html><head>")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            socket.set_zero_linger().unwrap();
            drop(socket);
        });
        let url: Url = format!("http://127.0.0.1:{}/page", addr.port())
            .parse()
            .unwrap();
        let fetched = PinnedTransport
            .get_with(&url, &[addr], &FetchOptions::default())
            .await
            .unwrap();
        assert_eq!((fetched.status, fetched.truncated), (403, true));
        assert_eq!(fetched.body, b"<html><head>");
        server.await.unwrap();
    }
    /// A TLS failure is an answer about the host (the certificate it serves, a protocol it does
    /// not speak): `connect`, but not unreached, so it still counts toward the dead letter.
    #[tokio::test]
    async fn a_tls_failure_is_not_unreached() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = [0u8; 4096];
            let _ = socket.read(&mut hello).await;
            let _ = socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                .await;
            let _ = socket.shutdown().await;
        });
        let url: Url = format!("https://127.0.0.1:{}/page", addr.port())
            .parse()
            .unwrap();
        let error = PinnedTransport
            .get_with(&url, &[addr], &FetchOptions::default())
            .await
            .unwrap_err();
        assert_eq!(super::super::error_class(&error), "connect", "{error:#}");
        assert!(!super::super::unreached(&error), "{error:#}");
        server.await.unwrap();
    }
    /// A large PDF or an image past the page cap is cut there, not refused
    /// URL. Any other file past the cap is still refused.
    #[tokio::test]
    async fn a_large_pdf_or_image_is_cut_not_refused() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let file = |content_type: &'static str| {
            move || async move { ([("content-type", content_type)], vec![b'%'; 3 * BODY_CAP]) }
        };
        let app = Router::new()
            .route("/paper.pdf", get(file("application/pdf")))
            .route("/photo.jpg", get(file("image/jpeg")))
            .route("/data.json", get(file("application/json")));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let read = |path: &str| {
            let url: Url = format!("http://{addr}{path}").parse().unwrap();
            let request = client.get(url.clone());
            async move {
                read_response_with(url, request.send().await.unwrap(), &Default::default()).await
            }
        };
        for path in ["/paper.pdf", "/photo.jpg"] {
            let fetched = read(path).await.unwrap();
            assert!(fetched.truncated, "{path}");
            assert_eq!(fetched.body.len(), BODY_CAP, "{path}");
        }
        let error = read("/data.json").await.unwrap_err();
        assert!(error.downcast_ref::<BodyTooLarge>().is_some(), "{error:#}");
        server.abort();
    }
    /// An RSS or Atom feed past the page cap keeps its first megabyte, marked truncated, or as
    /// much as the read asks for (`feed_read`), unless the fetch set a feed cap (calendars),
    /// which refuses what exceeds it.
    #[tokio::test]
    async fn a_long_xml_feed_keeps_its_first_megabyte() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async {
                let body = format!("<rss><channel>{}", "<item/>".repeat(2 * BODY_CAP / 7));
                ([("content-type", "application/rss+xml")], body)
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url: Url = format!("http://{addr}/").parse().unwrap();
        let client = reqwest::Client::new();
        let read = |options: FetchOptions| {
            let (url, request) = (url.clone(), client.get(url.clone()));
            async move { read_response_with(url, request.send().await.unwrap(), &options).await }
        };
        let fetched = read(Default::default()).await.unwrap();
        assert!(fetched.truncated);
        assert_eq!(fetched.body.len(), BODY_CAP);
        // A read that asks for more (a show's feed read for its episodes) goes on to it: cut
        // there, or whole within it; never less than the page cap.
        let further = |n: usize| FetchOptions {
            feed_read: Some(n),
            ..Default::default()
        };
        let longer = read(further(BODY_CAP + BODY_CAP / 2)).await.unwrap();
        assert!(longer.truncated);
        assert_eq!(longer.body.len(), BODY_CAP + BODY_CAP / 2);
        let whole = read(further(4 * BODY_CAP)).await.unwrap();
        assert!(!whole.truncated);
        assert_eq!(whole.body.len(), 14 + 7 * (2 * BODY_CAP / 7));
        let short = read(further(1024)).await.unwrap();
        assert_eq!((short.truncated, short.body.len()), (true, BODY_CAP));
        // Each is its own answer in the discovery cache.
        let key = |o: &FetchOptions| Egress::discovery_key(url.as_str(), o).unwrap();
        assert_ne!(key(&FetchOptions::default()), key(&further(4 * BODY_CAP)));
        assert_ne!(key(&further(2 * BODY_CAP)), key(&further(4 * BODY_CAP)));
        let capped = FetchOptions {
            feed_cap: Some(BODY_CAP),
            ..Default::default()
        };
        let error = read(capped).await.unwrap_err();
        assert!(error.downcast_ref::<BodyTooLarge>().is_some(), "{error:#}");
        // A feed cap refuses as before, whatever the read would take.
        let both = FetchOptions {
            feed_cap: Some(BODY_CAP),
            feed_read: Some(4 * BODY_CAP),
            ..Default::default()
        };
        let error = read(both).await.unwrap_err();
        assert!(error.downcast_ref::<BodyTooLarge>().is_some(), "{error:#}");
        server.abort();
    }
    /// A refusal is replayed only to reads with its cap: another cap asks again.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_replayed_to_its_cap_only() {
        let url = "https://www.wikidata.org/w/api.php?action=wbgetentities&ids=Q1";
        let (egress, recorded) =
            crate::sources::fixture::egress(vec![(url, 200, "!too-large", vec![])]);
        let read = |cap: usize| FetchOptions {
            body_cap: Some(cap),
            ..Default::default()
        };
        for (cap, asked) in [(8 * BODY_CAP, 1), (8 * BODY_CAP, 1), (16 * BODY_CAP, 2)] {
            let error = egress.discover(url, &read(cap), 3600).await.unwrap_err();
            assert!(over_cap(&error), "{error:#}");
            assert_eq!(recorded.asked().len(), asked, "cap {cap}");
        }
    }
    /// The refusal names the cap it hit.
    #[test]
    fn a_refused_body_names_its_cap() {
        assert_eq!(
            BodyTooLarge { cap: BODY_CAP }.to_string(),
            "body exceeds its cap of 1 MB"
        );
        assert_eq!(
            BodyTooLarge { cap: 8 * BODY_CAP }.to_string(),
            "body exceeds its cap of 8 MB"
        );
        assert_eq!(
            BodyTooLarge { cap: 8 * 1024 }.to_string(),
            "body exceeds its cap of 8192 bytes"
        );
    }
    /// A cap above the default admits a larger JSON body (a much-edited Wikidata item).
    #[tokio::test]
    async fn a_body_cap_above_the_default_admits_larger_json() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async {
                let body = format!("{{\"x\":\"{}\"}}", "x".repeat(3 * BODY_CAP));
                ([("content-type", "application/json")], body)
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url: Url = format!("http://{addr}/").parse().unwrap();
        let client = reqwest::Client::new();
        let read = |options: FetchOptions| {
            let (url, request) = (url.clone(), client.get(url.clone()));
            async move { read_response_with(url, request.send().await.unwrap(), &options).await }
        };
        let large = FetchOptions {
            body_cap: Some(8 * BODY_CAP),
            ..Default::default()
        };
        assert_eq!(read(large).await.unwrap().body.len(), 3 * BODY_CAP + 8);
        let error = read(Default::default()).await.unwrap_err();
        assert!(error.downcast_ref::<BodyTooLarge>().is_some(), "{error:#}");
        server.abort();
    }
    #[tokio::test(start_paused = true)]
    async fn graphql_error_expires_before_the_next_retry() {
        struct Graphql(AtomicUsize);
        #[async_trait]
        impl Transport for Graphql {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "application/json".into(),
                    if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                        br#"{"errors":[{"message":"temporary"}]}"#.to_vec()
                    } else {
                        br#"{"data":{"event":null}}"#.to_vec()
                    },
                ))
            }
        }
        let transport = Arc::new(Graphql(AtomicUsize::new(0)));
        let egress =
            Egress::with_backends(Default::default(), Arc::new(FakeDns), transport.clone());
        let options = FetchOptions {
            json_body: Some(serde_json::json!({"query":"fixture"})),
            ..Default::default()
        };
        let uri = "https://public.example/api";
        let definition = crate::event::adapters::definition("mobilizon").unwrap();
        for advance in [0, 299] {
            tokio::time::advance(Duration::from_secs(advance)).await;
            let error = crate::event::adapters::service::discover_enrichment(
                &egress, uri, &options, 86400, definition,
            )
            .await
            .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<super::super::RetryLater>()
                    .unwrap()
                    .class,
                "graphql"
            );
        }
        assert_eq!(transport.0.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        crate::event::adapters::service::discover_enrichment(
            &egress, uri, &options, 86400, definition,
        )
        .await
        .unwrap();
        assert_eq!(transport.0.load(Ordering::SeqCst), 2);
    }
    #[tokio::test(start_paused = true)]
    async fn a_failure_that_names_its_wait_is_cached_that_long() {
        let egress = Egress::new(Default::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let ask = |seconds: u64| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<Fetched, _>(anyhow::Error::new(super::super::RetryLater {
                    class: "http_4xx",
                    seconds,
                }))
            }
        };
        // A 429 that asked for a minute: replayed within it, asked again after it.
        assert!(egress
            .cached("busy".into(), 3600, &[], &[], ask(60))
            .await
            .is_err());
        let replay = egress
            .cached("busy".into(), 3600, &[], &[], async { panic!("replayed") })
            .await
            .unwrap_err();
        assert_eq!(
            replay
                .downcast_ref::<super::super::RetryLater>()
                .map(|r| r.class),
            Some("discovery_cached")
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(egress
            .cached("busy".into(), 3600, &[], &[], ask(60))
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // One that asked for an hour is still held to five minutes.
        assert!(egress
            .cached("long".into(), 3600, &[], &[], ask(3600))
            .await
            .is_err());
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(egress
            .cached("long".into(), 3600, &[], &[], ask(3600))
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
    /// The fetch that confirms a missing answer asks again: whatever its resolver reads as "not
    /// there" (a 404, a portal's 401 or 403, a 400, a 200 with nothing in it), the cached answer is
    /// not replayed to it, and its new answer is kept and replayed for the rest of that
    /// confirmation. An answer that names a wait (a 408, 429 or 5xx, a status the caller names in
    /// `waits`, a `RetryLater`) is replayed.
    #[tokio::test(start_paused = true)]
    async fn a_confirmation_asks_again_for_a_cached_failure() {
        let egress = Egress::new(Default::default());
        let answer = |status| {
            Fetched::answer(
                Url::parse("https://example.org/gone").unwrap(),
                status,
                "text/html".into(),
                vec![],
            )
        };
        let status = |r: Result<Fetched>| r.unwrap().status;
        egress
            .cached("gone".into(), 3600, &[], &[], async { Ok(answer(404)) })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(
            status(
                egress
                    .cached("gone".into(), 3600, &[], &[], async { panic!("replayed") })
                    .await
            ),
            404
        );
        // One confirmation asks once, and replays its new answer to its later reads.
        let (first, again) = confirming(async {
            (
                egress
                    .cached("gone".into(), 3600, &[], &[], async { Ok(answer(200)) })
                    .await,
                egress
                    .cached("gone".into(), 3600, &[], &[], async {
                        panic!("asked twice in one confirmation")
                    })
                    .await,
            )
        })
        .await;
        assert_eq!((status(first), status(again)), (200, 200));
        assert_eq!(
            status(
                egress
                    .cached("gone".into(), 3600, &[], &[], async { panic!("kept") })
                    .await
            ),
            200
        );
        // Any other answer is asked again by the next confirmation, a success too: a resolver
        // may read a page or a feed with nothing in it as gone.
        for code in [200, 400, 401, 403, 410, 451] {
            let key = format!("answered-{code}");
            egress
                .cached(key.clone(), 3600, &[], &[], async move { Ok(answer(code)) })
                .await
                .unwrap();
            assert_eq!(
                status(
                    confirming(egress.cached(key, 3600, &[], &[], async { Ok(answer(299)) })).await
                ),
                299,
                "a cached {code} was replayed to a confirmation"
            );
        }
        // A cached answer that names a wait (a 408, 429 or 503 with `Retry-After`, or a 403 its
        // caller reads as a rate limit) is replayed to a confirmation: its resolver turns it into
        // the wait.
        for (key, code, waits) in [
            ("timeout", 408, &[][..]),
            ("limited", 429, &[]),
            ("unavailable", 503, &[]),
            ("rate-limited", 403, &[403]),
        ] {
            egress
                .cached(
                    key.into(),
                    3600,
                    &[],
                    waits,
                    async move { Ok(answer(code)) },
                )
                .await
                .unwrap();
            assert_eq!(
                status(
                    confirming(egress.cached(key.into(), 3600, &[], waits, async {
                        panic!("a named wait is cut short")
                    }))
                    .await
                ),
                code
            );
        }
        egress
            .cached("busy".into(), 3600, &[], &[], async {
                Err(super::super::RetryLater {
                    class: "http_5xx",
                    seconds: 120,
                }
                .into())
            })
            .await
            .unwrap_err();
        let error = confirming(egress.cached("busy".into(), 3600, &[], &[], async {
            panic!("the named wait holds")
        }))
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<super::super::RetryLater>().is_some());
    }
    #[tokio::test(start_paused = true)]
    async fn discovery_capacity_evicts_lru_and_failures_expire_after_five_minutes() {
        let egress = Egress::new(Default::default());
        let now = Instant::now();
        {
            let mut cache = egress.discovered.lock().await;
            for i in 0..10000 {
                cache.insert(
                    i.to_string(),
                    DiscoveryEntry {
                        expires: now + Duration::from_secs(86400),
                        accessed: now - Duration::from_secs(10000 - i),
                        value: None,
                        too_large: None,
                    },
                );
            }
        }
        assert!(egress
            .cached("0".into(), 86400, &[], &[], async { panic!("cached") })
            .await
            .is_err());
        let response = Fetched::answer(
            Url::parse("https://example.org/").unwrap(),
            200,
            "text/html".into(),
            vec![1],
        );
        egress
            .cached("fresh".into(), 86400, &[], &[], async {
                Ok(response.clone())
            })
            .await
            .unwrap();
        {
            let cache = egress.discovered.lock().await;
            assert_eq!(cache.len(), 10000);
            assert!(cache.contains_key("0"));
            assert!(!cache.contains_key("1"));
        }
        egress
            .cached("failure".into(), 86400, &[], &[], async {
                bail!("transport")
            })
            .await
            .unwrap_err();
        tokio::time::advance(Duration::from_secs(299)).await;
        let error = egress
            .cached("failure".into(), 86400, &[], &[], async {
                panic!("negative cache")
            })
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RetryLater>()
                .unwrap()
                .seconds,
            1
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        egress
            .cached("failure".into(), 86400, &[], &[], async {
                Ok(response.clone())
            })
            .await
            .unwrap();
        egress
            .cached("fresh".into(), 86400, &[], &[], async {
                panic!("positive TTL remains")
            })
            .await
            .unwrap();
        // HTTP errors also expire quickly, and a body replaces old bodies within the byte cap.
        let mut failure = response.clone();
        failure.status = 503;
        egress
            .cached("http-error".into(), 86400, &[], &[], async { Ok(failure) })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(301)).await;
        egress
            .cached("http-error".into(), 86400, &[], &[], async { Ok(response) })
            .await
            .unwrap();
        for key in ["large-a", "large-b"] {
            egress
                .cached(key.into(), 3600, &[], &[], async {
                    Ok(Fetched::answer(
                        Url::parse("https://example.org/").unwrap(),
                        200,
                        "text/calendar".into(),
                        vec![0; 20 * 1024 * 1024],
                    ))
                })
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        let cache = egress.discovered.lock().await;
        assert!(!cache.contains_key("large-a"));
        assert_eq!(
            cache
                .values()
                .filter_map(|e| e.value.as_ref())
                .map(|f| f.body.len())
                .sum::<usize>(),
            20 * 1024 * 1024
        );
    }
    /// An answer keeps its `Content-Type` charset; its text is decoded by a byte order mark,
    /// that charset, or a `<meta>` within the first 1024 bytes, else as UTF-8.
    #[tokio::test]
    async fn an_answer_s_text_is_decoded_by_its_charset() {
        let url = Url::parse("https://example.org/page").unwrap();
        let japanese = "<title>日本語</title>";
        let (shift_jis, _, _) = encoding_rs::SHIFT_JIS.encode(japanese);
        let response = axum::http::Response::builder()
            .status(200)
            .header("content-type", "text/html; Charset=\"Shift_JIS\"")
            .body(shift_jis.to_vec())
            .unwrap();
        let fetched = read_response_with(
            url.clone(),
            reqwest::Response::from(response),
            &FetchOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(fetched.content_type, "text/html");
        assert_eq!(fetched.charset.as_deref(), Some("shift_jis"));
        assert_eq!(fetched.text(), japanese);
        let meta = |padding: usize| {
            let mut body =
                format!("{}<meta charset='shift_jis'>", " ".repeat(padding)).into_bytes();
            body.extend_from_slice(&shift_jis);
            decode(&body, None).into_owned()
        };
        assert!(meta(0).ends_with(japanese));
        // Past the first 1024 bytes a declaration is not looked for: UTF-8.
        assert!(!meta(1100).contains(japanese));
        // Only a declaration is one: a tag's own `charset`, or an http-equiv `content`; never
        // a comment, another attribute or a `<meta>` inside another tag's attribute.
        let declared = |head: &str| {
            let mut body = head.as_bytes().to_vec();
            body.extend_from_slice(&shift_jis);
            decode(&body, None).ends_with(japanese)
        };
        for head in [
            "<meta name=viewport charset=\"Shift_JIS\">",
            "<META HTTP-EQUIV=\"Content-Type\" CONTENT=\"text/html; charset=Shift_JIS\">",
            "<meta content='text/html; charset=\"shift_jis\"' http-equiv=content-type>",
            "<!-- a note --><meta charset=shift_jis />",
        ] {
            assert!(declared(head), "{head}");
        }
        for head in [
            "<!-- <meta charset=shift_jis> -->",
            "<meta data-charset=shift_jis>",
            "<meta content=\"text/html; charset=shift_jis\">",
            "<meta name=\"description\" content=\"charset=shift_jis\">",
            "<div title=\"<meta charset=shift_jis>\"></div>",
            "<metadata charset=shift_jis>",
            "<meta http-equiv=content-type content=\"text/html; charset='shift_jis\">",
            "<meta charset=bogus http-equiv=content-type content=\"text/html; charset=shift_jis\">",
        ] {
            assert!(!declared(head), "{head}");
        }
        assert_eq!(decode("Grüße".as_bytes(), None), "Grüße");
        assert_eq!(decode(b"Gr\xfc\xdfe", Some("latin1")), "Grüße");
        assert_eq!(
            decode(
                b"<meta http-equiv=content-type content='text/html;charset=utf-16'>\xc3\xa9",
                None
            ),
            "<meta http-equiv=content-type content='text/html;charset=utf-16'>é"
        );
    }

    /// A spent quota (GitHub's rate limit: `X-RateLimit-Remaining: 0`) is waited for until its
    /// `X-RateLimit-Reset`, at most an hour; a `Retry-After` still wins, and a quota not spent
    /// names no wait.
    #[tokio::test]
    async fn a_spent_quota_is_waited_for_until_its_reset() {
        let now = std::time::SystemTime::now();
        let epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let options = FetchOptions::default();
        let answer = |status: u16, headers: &[(&str, String)]| {
            let mut response = axum::http::Response::builder()
                .status(status)
                .header("content-type", "text/plain");
            for (name, value) in headers {
                response = response.header(*name, value);
            }
            let response = response.body(b"{}".to_vec()).unwrap();
            read_response_with(
                Url::parse("https://api.example.org/").unwrap(),
                reqwest::Response::from(response),
                &options,
            )
        };
        let spent = |reset: u64| {
            vec![
                ("x-ratelimit-remaining", "0".to_owned()),
                ("x-ratelimit-reset", reset.to_string()),
            ]
        };
        for status in [403, 429] {
            let f = answer(status, &spent(epoch + 900)).await.unwrap();
            assert!(
                (895..=900).contains(&f.retry_after_secs),
                "{status}: {}",
                f.retry_after_secs
            );
        }
        let f = answer(403, &spent(epoch + 86400)).await.unwrap();
        assert_eq!(f.retry_after_secs, 3600);
        let mut named = spent(epoch + 900);
        named.push(("retry-after", "120".to_owned()));
        assert_eq!(answer(403, &named).await.unwrap().retry_after_secs, 120);
        let left = vec![
            ("x-ratelimit-remaining", "12".to_owned()),
            ("x-ratelimit-reset", (epoch + 900).to_string()),
        ];
        assert_eq!(answer(403, &left).await.unwrap().retry_after_secs, 60);
    }

    /// The Fetch standard's bad ports are refused before any lookup or request, on a link, a
    /// redirect's target and a relay connection; other ports (a homeserver's 6286) are asked.
    #[tokio::test]
    async fn bad_ports_are_never_asked() {
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "https://example.org:25/",
        });
        let egress = Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            transport.clone(),
        );
        for uri in [
            "http://example.org:22/",
            "https://example.org:25/",
            "http://example.org:6667/",
            "http://example.org:0/",
        ] {
            let error = egress.fetch(uri).await.unwrap_err();
            assert_eq!(crate::resolver::error_class(&error), "policy", "{uri}");
            assert!(
                format!("{error:#}").contains("port blocked"),
                "{uri}: {error:#}"
            );
        }
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
        // A redirect to a bad port is refused at that hop.
        let error = egress.fetch("https://example.org:6286/").await.unwrap_err();
        assert!(format!("{error:#}").contains("port blocked"), "{error:#}");
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let error = egress
            .nostr_request_inner("wss://relay.example:25", serde_json::json!({}), 1000)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("port blocked"), "{error:#}");
        assert!(BAD_PORTS.windows(2).all(|w| w[0] < w[1]));
    }

    /// Two names on one address are paced together: their requests never overlap. Names on two
    /// addresses do, and so do two services' own hosts on one address (a source's API host, a
    /// published one), which no link multiplies; a `*.` rule's names are paced by address.
    #[tokio::test(start_paused = true)]
    async fn two_names_on_one_address_do_not_overlap() {
        struct Shared(bool);
        #[async_trait]
        impl Dns for Shared {
            async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
                let ip = if self.0 || host == "a.example.org" {
                    "8.8.8.8"
                } else {
                    "8.8.4.4"
                };
                Ok(vec![SocketAddr::new(ip.parse()?, port)])
            }
        }
        #[derive(Default)]
        struct Slow {
            now: AtomicUsize,
            most: AtomicUsize,
        }
        #[async_trait]
        impl Transport for Slow {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
                self.most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(500)).await;
                self.now.fetch_sub(1, Ordering::SeqCst);
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "text/plain".into(),
                    b"ok".to_vec(),
                ))
            }
        }
        for (shared, a, b, most) in [
            (true, "a.example.org", "b.example.org", 1),
            (false, "a.example.org", "b.example.org", 2),
            (true, "api.one.example", "api.two.example", 2),
            (true, "www.wikidata.org", "commons.wikimedia.org", 2),
            (true, "de.wikipedia.org", "fr.wikipedia.org", 1),
        ] {
            let transport = Arc::new(Slow::default());
            let egress = Egress::with_backends(
                EgressConfig {
                    host_requests_per_second: 1000.0,
                    source_rates: [
                        ("api.one.example".to_owned(), 1000.0),
                        ("api.two.example".to_owned(), 1000.0),
                    ]
                    .into(),
                    ..Default::default()
                },
                Arc::new(Shared(shared)),
                transport.clone(),
            );
            let (a_url, b_url) = (format!("https://{a}/"), format!("https://{b}/"));
            let (one, two) = tokio::join!(egress.fetch(&a_url), egress.fetch(&b_url));
            one.unwrap();
            two.unwrap();
            assert_eq!(
                transport.most.load(Ordering::SeqCst),
                most,
                "{a} {b} shared {shared}"
            );
        }
    }

    /// A same-host redirect whose new DNS answer is another address takes that address's slot:
    /// it never overlaps another name's request to that address.
    #[tokio::test(start_paused = true)]
    async fn a_redirect_to_another_address_takes_its_slot() {
        #[derive(Default)]
        struct Moving(AtomicUsize);
        #[async_trait]
        impl Dns for Moving {
            async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
                // a.example.org answers 8.8.8.8 first, then 8.8.4.4 (b.example.org's).
                let ip = if host == "a.example.org" && self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    "8.8.8.8"
                } else {
                    "8.8.4.4"
                };
                Ok(vec![SocketAddr::new(ip.parse()?, port)])
            }
        }
        #[derive(Default)]
        struct Slow {
            at: std::sync::Mutex<HashMap<IpAddr, usize>>,
            most: AtomicUsize,
        }
        #[async_trait]
        impl Transport for Slow {
            async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
                let ip = addresses[0].ip();
                let now = {
                    let mut at = self.at.lock().unwrap();
                    let now = at.entry(ip).or_default();
                    *now += 1;
                    *now
                };
                self.most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(500)).await;
                *self.at.lock().unwrap().get_mut(&ip).unwrap() -= 1;
                Ok(
                    if url.path() == "/" && url.host_str() == Some("a.example.org") {
                        Fetched {
                            location: Some("/next".into()),
                            ..Fetched::answer(url.clone(), 302, "text/plain".into(), vec![])
                        }
                    } else {
                        Fetched::answer(url.clone(), 200, "text/plain".into(), b"ok".to_vec())
                    },
                )
            }
        }
        let transport = Arc::new(Slow::default());
        let egress = Egress::with_backends(
            EgressConfig {
                host_requests_per_second: 1000.0,
                ..Default::default()
            },
            Arc::new(Moving::default()),
            transport.clone(),
        );
        // a's first hop runs 0–500 ms on 8.8.8.8, its second 500–1000 ms on 8.8.4.4; b asks
        // 8.8.4.4 at 600 ms.
        let (a, b) = tokio::join!(egress.fetch("https://a.example.org/"), async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            egress.fetch("https://b.example.org/").await
        });
        assert_eq!(a.unwrap().url.path(), "/next");
        b.unwrap();
        assert_eq!(transport.most.load(Ordering::SeqCst), 1);
    }

    /// A hub's published limit lowers its host's default pace; an operator's entry sets it.
    #[tokio::test(start_paused = true)]
    async fn a_published_limit_paces_its_host() {
        assert_eq!(published_rate("musicbrainz.org"), Some(1.0));
        assert_eq!(published_rate("de.wikipedia.org"), Some(200.0 / 60.0));
        assert_eq!(published_rate("wikipedia.org"), Some(200.0 / 60.0));
        assert_eq!(published_rate("evilwikipedia.org"), None);
        assert_eq!(published_rate("example.org"), None);
        let gap = |cfg: EgressConfig, host: &'static str| async move {
            let egress = Egress::new(cfg);
            drop(egress.host_permit(host).await.unwrap());
            let start = Instant::now();
            drop(egress.host_permit(host).await.unwrap());
            start.elapsed()
        };
        // iTunes allows about 20 requests a minute: three seconds apart, not the default one.
        let itunes = gap(EgressConfig::default(), "itunes.apple.com").await;
        assert!(itunes >= Duration::from_secs(3), "{itunes:?}");
        // A published limit above the default never raises it.
        let crossref = gap(EgressConfig::default(), "api.crossref.org").await;
        assert!(crossref >= Duration::from_secs(1), "{crossref:?}");
        let mut slow = EgressConfig::default();
        slow.host_rates.insert("www.wikidata.org".into(), 0.5);
        let wd = gap(slow, "www.wikidata.org").await;
        assert!(wd >= Duration::from_secs(2), "{wd:?}");
        // The indexers' and the portal's pace is a published rate like any other: it lowers a
        // higher default, and an operator's entry sets the host's rate.
        let fast = || EgressConfig {
            host_requests_per_second: 10.0,
            ..Default::default()
        };
        for (host, rate) in [
            ("api.nostr.wine", 1.0),
            ("api.nostrarchives.com", 2.0),
            ("portal.einundzwanzig.space", 1.0),
        ] {
            assert_eq!(Egress::new(fast()).host_rate(host), rate, "{host}");
            let mut own = fast();
            own.host_rates.insert(host.into(), 5.0);
            assert_eq!(Egress::new(own).host_rate(host), 5.0, "{host}");
        }
    }
    #[test]
    fn rejects_every_special_range() {
        for ip in [
            "0.1.2.3",
            "10.1.2.3",
            "100.64.0.1",
            "100.100.100.200",
            "127.255.1.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.1.1",
            "240.0.0.1",
            "255.255.255.255",
            "168.63.129.16",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "100::1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
    struct FakeDns;
    #[async_trait]
    impl Dns for FakeDns {
        async fn lookup(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
            Ok(vec![SocketAddr::new(
                if host == "private.example" {
                    "127.0.0.1"
                } else {
                    "8.8.8.8"
                }
                .parse()?,
                port,
            )])
        }
    }
    struct Redirect {
        calls: AtomicUsize,
        target: &'static str,
    }
    #[async_trait]
    impl Transport for Redirect {
        async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
            assert!(addresses.iter().all(|s| public_ip(s.ip())));
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Fetched {
                location: Some(self.target.into()),
                ..Fetched::answer(url.clone(), 302, "text/html".into(), vec![])
            })
        }
    }
    #[tokio::test]
    async fn redirects_recheck_dns_and_stop_at_three() {
        let cfg = EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        };
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "http://private.example/",
        });
        let egress = Egress::with_backends(cfg.clone(), Arc::new(FakeDns), transport.clone());
        assert!(egress
            .fetch("https://public.example/")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-public"));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "https://public.example/",
        });
        let egress = Egress::with_backends(cfg, Arc::new(FakeDns), transport.clone());
        assert!(egress
            .fetch("https://public.example/")
            .await
            .unwrap_err()
            .to_string()
            .contains("3 redirects"));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 4);
    }
    #[test]
    fn allowlist_never_matches_suffix_impostors() {
        assert!(host_matches("a.example.org", "*.example.org"));
        assert!(!host_matches("evilexample.org", "*.example.org"));
    }
    #[tokio::test]
    async fn body_cap_catches_streams_and_content_length() {
        use axum::{
            body::Body,
            http::{header, Response},
            routing::get,
            Router,
        };
        for length_header in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app = Router::new().route(
                "/",
                get(move || async move {
                    let mut response =
                        Response::builder().header(header::CONTENT_TYPE, "text/html");
                    if length_header {
                        response =
                            response.header(header::CONTENT_LENGTH, (BODY_CAP + 1).to_string());
                    }
                    let chunks = futures_util::stream::iter(
                        (0..1025).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024])),
                    );
                    response.body(Body::from_stream(chunks)).unwrap()
                }),
            );
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let url = Url::parse(&format!("http://{addr}/")).unwrap();
            let response = reqwest::get(url.clone()).await.unwrap();
            let f = read_response_with(url, response, &Default::default())
                .await
                .unwrap();
            assert!(f.truncated);
            assert_eq!(f.body.len(), BODY_CAP);
            task.abort();
        }
    }
    #[tokio::test]
    async fn mixed_dns_answers_literals_and_allowlists_cannot_bypass_ip_policy() {
        struct MixedDns;
        #[async_trait]
        impl Dns for MixedDns {
            async fn lookup(&self, _: &str, port: u16) -> Result<Vec<SocketAddr>> {
                Ok(vec![
                    SocketAddr::new("8.8.8.8".parse()?, port),
                    SocketAddr::new("10.0.0.1".parse()?, port),
                ])
            }
        }
        let transport = Arc::new(Redirect {
            calls: AtomicUsize::new(0),
            target: "/",
        });
        let cfg = EgressConfig {
            allow_hosts: vec!["*.example.org".into()],
            ..Default::default()
        };
        let egress = Egress::with_backends(cfg, Arc::new(MixedDns), transport.clone());
        assert!(egress
            .fetch("https://a.example.org")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-public"));
        assert!(egress
            .fetch("https://example.net")
            .await
            .unwrap_err()
            .to_string()
            .contains("operator"));
        let egress = Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            transport.clone(),
        );
        for uri in [
            "http://127.1/",
            "http://0x7f000001/",
            "http://2130706433/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::1]/",
            "file:///etc/passwd",
        ] {
            assert!(egress.fetch(uri).await.is_err(), "{uri}");
        }
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
    /// A credential in the query (`query_secret`) is the wire's alone: the transport is handed
    /// the URL without it (and the credential, to add), the answer's URL and the cache's key hold
    /// none, a plain request may not carry one, and its Debug hides it.
    #[tokio::test]
    async fn a_query_credential_is_on_the_wire_alone() {
        #[derive(Default)]
        struct Seen(std::sync::Mutex<Vec<(String, String)>>);
        #[async_trait]
        impl Transport for Seen {
            async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &Url,
                _: &[SocketAddr],
                options: &FetchOptions,
            ) -> Result<Fetched> {
                self.0
                    .lock()
                    .unwrap()
                    .push((url.to_string(), wire_url(url, options).to_string()));
                Ok(Fetched::answer(
                    url.clone(),
                    200,
                    "application/json".into(),
                    b"{}".to_vec(),
                ))
            }
        }
        let transport = Arc::new(Seen::default());
        let egress = Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            transport.clone(),
        );
        let url = "https://api.example.org/3/find/tt1?external_source=imdb_id";
        let options = FetchOptions {
            query_secret: Some(("api_key", "SECRETKEY".into())),
            sensitive: true,
            ..Default::default()
        };
        let fetched = egress.discover(url, &options, 60).await.unwrap();
        assert_eq!(
            *transport.0.lock().unwrap(),
            [(url.to_owned(), format!("{url}&api_key=SECRETKEY"))]
        );
        assert!(!format!("{fetched:?}").contains("SECRETKEY"));
        assert!(!Egress::discovery_key(url, &options)
            .unwrap()
            .contains("SECRETKEY"));
        assert!(!format!("{options:?}").contains("SECRETKEY"));
        // Cached by the URL alone: asked once.
        egress.discover(url, &options, 60).await.unwrap();
        assert_eq!(transport.0.lock().unwrap().len(), 1);
        let plain = FetchOptions {
            sensitive: false,
            ..options
        };
        let error = egress.fetch_with(url, &plain).await.unwrap_err();
        assert_eq!(super::super::error_class(&error), "policy");
    }
    /// A refused connection on a request with credentials is rebuilt without its cause, and still
    /// reads as one that never reached the upstream: the queue never dead-letters on it.
    #[tokio::test]
    async fn a_refused_request_with_credentials_stays_unreached() {
        struct Closed(SocketAddr);
        #[async_trait]
        impl Transport for Closed {
            async fn get(&self, url: &Url, addresses: &[SocketAddr]) -> Result<Fetched> {
                self.get_with(url, addresses, &FetchOptions::default())
                    .await
            }
            async fn get_with(
                &self,
                url: &Url,
                _: &[SocketAddr],
                options: &FetchOptions,
            ) -> Result<Fetched> {
                PinnedTransport.get_with(url, &[self.0], options).await
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);
        let egress = Egress::with_backends(
            EgressConfig::default(),
            Arc::new(FakeDns),
            Arc::new(Closed(closed)),
        );
        let options = FetchOptions {
            query_secret: Some(("api_key", "SECRETKEY".into())),
            sensitive: true,
            ..Default::default()
        };
        let error = egress
            .fetch_with("https://api.example.org/3/find/tt1", &options)
            .await
            .unwrap_err();
        assert!(!format!("{error:#} {error:?}").contains("SECRETKEY"));
        assert_eq!(super::super::error_class(&error), "connect");
        assert!(super::super::unreached(&error));
    }
    #[test]
    fn a_sensitive_request_s_debug_hides_its_credentials_and_two_bodies_are_refused() {
        let options = FetchOptions {
            headers: vec![("authorization", "Bearer SECRETTOKEN".into())],
            form_body: Some(vec![("client_secret", "SECRETPAIR".into())]),
            sensitive: true,
            ..Default::default()
        };
        let shown = format!("{options:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(
            shown.contains("authorization") && shown.contains("client_secret"),
            "{shown}"
        );
        let plain = FetchOptions {
            headers: vec![("x-request-id", "abc".into())],
            ..Default::default()
        };
        assert!(format!("{plain:?}").contains("abc"));
        // A cache tag keys the cache in place of the headers, and is never printed.
        let tagged = |token: &str, tag: &str| FetchOptions {
            headers: vec![("authorization", format!("Bearer {token}"))],
            cache_tag: Some(tag.into()),
            sensitive: true,
            ..Default::default()
        };
        let key = |o: &FetchOptions| Egress::discovery_key("https://example.org/a", o).unwrap();
        assert_eq!(key(&tagged("old", "pair")), key(&tagged("new", "pair")));
        assert_ne!(key(&tagged("old", "pair")), key(&tagged("old", "other")));
        assert!(!format!("{:?}", tagged("TOKEN", "PAIRTAG")).contains("PAIRTAG"));
        // A fetched answer's Debug never shows its body.
        let fetched = Fetched::answer(
            Url::parse("https://auth.example/token").unwrap(),
            200,
            "application/json".into(),
            br#"{"access_token":"SECRETTOKEN"}"#.to_vec(),
        );
        let shown = format!("{fetched:?}");
        assert!(
            !shown.contains("SECRETTOKEN") && shown.contains("<30 bytes>"),
            "{shown}"
        );
        let both = FetchOptions {
            json_body: Some(serde_json::json!({})),
            form_body: Some(vec![]),
            ..Default::default()
        };
        let error = Egress::discovery_key("https://example.org/", &both).unwrap_err();
        assert_eq!(super::super::error_class(&error), "policy");
    }
    #[tokio::test]
    async fn an_uncached_status_is_never_kept() {
        struct Counting(AtomicUsize);
        #[async_trait]
        impl Transport for Counting {
            async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Fetched::answer(
                    url.clone(),
                    // A 401, then a 200.
                    if n == 0 { 401 } else { 200 },
                    "application/json".into(),
                    b"{}".to_vec(),
                ))
            }
        }
        let transport = Arc::new(Counting(AtomicUsize::new(0)));
        let egress =
            Egress::with_backends(Default::default(), Arc::new(FakeDns), transport.clone());
        let options = FetchOptions {
            cache_tag: Some("pair".into()),
            uncached: &[401, 403],
            ..Default::default()
        };
        let uri = "https://public.example/item";
        assert_eq!(
            egress.discover(uri, &options, 3600).await.unwrap().status,
            401
        );
        assert!(!egress.discovered(uri, &options).await.unwrap());
        // Asked again, and the 200 is kept.
        assert_eq!(
            egress.discover(uri, &options, 3600).await.unwrap().status,
            200
        );
        assert_eq!(
            egress.discover(uri, &options, 3600).await.unwrap().status,
            200
        );
        assert_eq!(transport.0.load(Ordering::SeqCst), 2);
    }
    /// A link that looks like a feed asks for feed types first; any other link, for HTML first.
    /// A server that answers a feed's URL with a 404 when HTML leads (as jsonfeed.org does) then
    /// gives the feed.
    #[tokio::test]
    async fn a_feeds_link_asks_for_feeds_first() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut seen = vec![];
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = vec![0; 4096];
                let n = socket.read(&mut data).await.unwrap();
                let request = String::from_utf8_lossy(&data[..n]).to_lowercase();
                let accept = request
                    .lines()
                    .find_map(|l| l.strip_prefix("accept: "))
                    .unwrap_or("")
                    .to_owned();
                let answer = if accept.starts_with("text/html") {
                    "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                };
                socket.write_all(answer.as_bytes()).await.unwrap();
                seen.push(accept);
            }
            seen
        });
        let url = |path: &str| {
            Url::parse(&format!(
                "http://never-resolve.invalid:{}{path}",
                addr.port()
            ))
            .unwrap()
        };
        assert_eq!(
            PinnedTransport
                .get(&url("/feed.json"), &[addr])
                .await
                .unwrap()
                .status,
            200
        );
        assert_eq!(
            PinnedTransport
                .get(&url("/about"), &[addr])
                .await
                .unwrap()
                .status,
            404
        );
        let seen = task.await.unwrap();
        assert!(seen[0].starts_with("application/atom+xml"), "{seen:?}");
        assert!(seen[1].starts_with("text/html"), "{seen:?}");
        for (path, feed) in [
            ("/feed.xml", true),
            ("/pubky/releases.atom", true),
            ("/index.rss", true),
            ("/blog/feed/", true),
            ("/rss", true),
            ("/feed.json", true),
            ("/news/index.rdf", true),
            ("/", false),
            ("/feeding-tips", false),
            ("/posts/2026/hello.html", false),
            ("/api/items.json", false),
        ] {
            assert_eq!(
                default_accept(&url(path)).starts_with("application/atom+xml"),
                feed,
                "{path}"
            );
        }
    }
    /// An RSS 1.0 feed served as `application/rdf+xml` is read, as RSS and Atom are; a type
    /// nothing reads is still refused.
    #[tokio::test]
    async fn an_rdf_feed_is_admitted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for content_type in ["application/rdf+xml", "application/x-shockwave-flash"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = vec![0; 4096];
                let _ = socket.read(&mut data).await.unwrap();
                socket
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: 9\r\nConnection: close\r\n\r\n<rdf:RDF>").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let url = Url::parse(&format!(
            "http://never-resolve.invalid:{}/index.rdf",
            addr.port()
        ))
        .unwrap();
        let rdf = PinnedTransport.get(&url, &[addr]).await.unwrap();
        assert_eq!((rdf.status, rdf.body.as_slice()), (200, &b"<rdf:RDF>"[..]));
        let refused = PinnedTransport.get(&url, &[addr]).await.unwrap_err();
        assert!(refused.to_string().contains("not allowed"), "{refused:#}");
        task.await.unwrap();
    }
    #[tokio::test]
    async fn a_429_without_retry_after_waits_five_minutes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (header, wait) in [("", UNNAMED_429_WAIT), ("Retry-After: 30\r\n", 30)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = vec![0; 4096];
                let _ = socket.read(&mut data).await.unwrap();
                socket
                    .write_all(
                        format!("HTTP/1.1 429 Too Many Requests\r\n{header}Content-Length: 0\r\nConnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let url =
                Url::parse(&format!("http://never-resolve.invalid:{}/", addr.port())).unwrap();
            let response = PinnedTransport.get(&url, &[addr]).await.unwrap();
            assert_eq!(
                (response.status, response.retry_after_secs),
                (429, wait),
                "{header:?}"
            );
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn a_form_body_is_posted_form_encoded() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = vec![0; 4096];
            let n = socket.read(&mut data).await.unwrap();
            let request = String::from_utf8_lossy(&data[..n]).to_string();
            assert!(request.starts_with("POST /token HTTP/1.1"), "{request}");
            assert!(request
                .to_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"));
            assert!(
                request.ends_with("\r\n\r\ngrant_type=client_credentials"),
                "{request}"
            );
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let url = Url::parse(&format!(
            "http://never-resolve.invalid:{}/token",
            addr.port()
        ))
        .unwrap();
        let options = FetchOptions {
            form_body: Some(vec![("grant_type", "client_credentials".into())]),
            ..Default::default()
        };
        let response = PinnedTransport
            .get_with(&url, &[addr], &options)
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn transport_pins_dns_sends_fixed_agent_and_does_not_follow_or_store_cookies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = vec![0; 4096];
            let n = socket.read(&mut data).await.unwrap();
            let request = String::from_utf8_lossy(&data[..n]).to_lowercase();
            assert!(request.contains(&format!("user-agent: {}", USER_AGENT.to_lowercase())));
            assert!(!request.contains("cookie:"));
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/forbidden\r\nSet-Cookie: test=secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let url = Url::parse(&format!("http://never-resolve.invalid:{}/", addr.port())).unwrap();
        let response = PinnedTransport.get(&url, &[addr]).await.unwrap();
        assert_eq!(response.status, 302);
        task.await.unwrap();
    }
    #[tokio::test]
    async fn every_link_header_is_kept() {
        use axum::{http::HeaderMap, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async {
                        let mut headers = HeaderMap::new();
                        headers.insert("content-type", "text/html".parse().unwrap());
                        for value in [
                            "<https://w.example/wp-json/>; rel=\"https://api.w.org/\"",
                            "<https://w.example/wp-json/wp/v2/posts/7>; rel=\"alternate\"; type=\"application/json\"",
                        ] {
                            headers.append("link", value.parse().unwrap());
                        }
                        headers.append("link", "x".repeat(3000).parse().unwrap());
                        (headers, "<html></html>")
                    }),
                ),
            )
            .await
            .unwrap()
        });
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let response = reqwest::get(url.clone()).await.unwrap();
        let fetched = read_response_with(url, response, &Default::default())
            .await
            .unwrap();
        // Whole headers only, up to 2 KB.
        assert_eq!(
            fetched.validators.link.as_deref(),
            Some("<https://w.example/wp-json/>; rel=\"https://api.w.org/\", <https://w.example/wp-json/wp/v2/posts/7>; rel=\"alternate\"; type=\"application/json\"")
        );
        task.abort();
    }
    #[tokio::test]
    async fn non_allowlisted_content_type_is_refused_before_reading() {
        use axum::{routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async { ([("content-type", "application/octet-stream")], "binary") }),
                ),
            )
            .await
            .unwrap()
        });
        let url = Url::parse(&format!("http://{addr}/")).unwrap();
        let response = reqwest::get(url.clone()).await.unwrap();
        let error = read_response_with(url.clone(), response, &Default::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("content type"));
        // A refusal by policy, not broken content.
        assert_eq!(super::super::error_class(&error), "policy");
        // Pubky object fetches accept it.
        let options = FetchOptions {
            extra_types: &["application/octet-stream"],
            ..Default::default()
        };
        let fetched = read_response_with(url.clone(), reqwest::get(url).await.unwrap(), &options)
            .await
            .unwrap();
        assert_eq!(fetched.body, b"binary");
        task.abort();
    }

    /// The wall check asks for a parse job only for an answer it parses: HTML, or a challenge
    /// status that comes as plain text or without a type. Any other answer (an API's JSON, a
    /// feed) is settled by its type, without a job. Either way the answer comes back as fetched.
    #[tokio::test(start_paused = true)]
    async fn only_an_answer_the_wall_check_parses_asks_for_a_parse_job() {
        let page: &[u8] = b"<html><head><title>Page</title></head><body><p>Text</p></body></html>";
        // The address, the answer (status, type, body), the parse jobs asked for, and whether
        // the answer is a wall.
        let routes = vec![
            // An API's answers, whatever they say and whatever their status.
            (
                "https://api.example/record",
                200,
                "application/json",
                br#"{"title":"Just a moment","note":"Verify you are human"}"#.as_slice(),
                0,
                false,
            ),
            (
                "https://api.example/refused",
                403,
                "application/json",
                br#"{"error":"Verify you are human"}"#.as_slice(),
                0,
                false,
            ),
            (
                "https://api.example/feed",
                200,
                "application/rss+xml",
                b"<rss><channel><title>Just a moment</title></channel></rss>".as_slice(),
                0,
                false,
            ),
            // Plain text or no type on an ordinary status is content too.
            (
                "https://site.example/notice",
                200,
                "text/plain",
                b"Verify you are human".as_slice(),
                0,
                false,
            ),
            ("https://site.example/untyped", 200, "", page, 0, false),
            // HTML is parsed, a page and a wall alike.
            (
                "https://site.example/page",
                200,
                "text/html; charset=utf-8",
                page,
                1,
                false,
            ),
            (
                "https://site.example/wall",
                503,
                "text/html",
                b"<title>Just a moment</title>".as_slice(),
                1,
                true,
            ),
            // A challenge status as plain text or without a type is parsed as well.
            (
                "https://site.example/plain-wall",
                403,
                "text/plain",
                b"Verify you are human".as_slice(),
                1,
                true,
            ),
            (
                "https://site.example/untyped-wall",
                403,
                "",
                b"<title>Just a moment</title>".as_slice(),
                1,
                true,
            ),
        ];
        let (egress, _) = crate::sources::fixture::egress_raw(
            routes
                .iter()
                .map(|(url, status, kind, body, ..)| (*url, *status, *kind, body.to_vec()))
                .collect(),
        );
        for (url, status, kind, body, jobs, blocked) in routes {
            let before = crate::resolver::html::counted::jobs();
            let answer = egress.fetch(url).await;
            assert_eq!(
                crate::resolver::html::counted::jobs() - before,
                jobs,
                "{url}"
            );
            match answer {
                Ok(fetched) => {
                    assert!(!blocked, "{url}");
                    assert_eq!(
                        (
                            fetched.url.as_str(),
                            fetched.status,
                            fetched.content_type.as_str(),
                            fetched.body.as_slice(),
                            fetched.permanent_redirect,
                            fetched.truncated,
                        ),
                        (url, status, kind, body, false, false),
                        "{url}"
                    );
                }
                Err(error) => {
                    assert!(
                        blocked && error.is::<crate::resolver::interstitial::Blocked>(),
                        "{url}: {error:#}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod osm_policy_tests {
    use super::*;
    #[test]
    fn osm_hosts_and_disabled_canaries_are_refused() {
        let egress = Egress::new(EgressConfig::default());
        for uri in [
            "https://www.openstreetmap.org/user/alice",
            "https://osm.org/",
            "https://wiki.openstreetmap.org./node/1",
        ] {
            assert!(!egress.allows(uri));
        }
        assert!(egress.allows("https://openstreetmap.org.example.net/"));
        let web = crate::resolver::web::WebResolver {
            oembed_policy: Default::default(),
            cards: None,
            events: None,
            egress: Egress::new(EgressConfig {
                allow_hosts: vec!["only.example".into()],
                ..Default::default()
            }),
            config: Default::default(),
        };
        assert!(!crate::resolver::Resolver::canary_allowed(&web));
        assert!(crate::adapter::manifest("web").expected_fields.is_empty());
    }
}

#[cfg(test)]
mod relay_protocol_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        tungstenite::{protocol::Role, Message},
        WebSocketStream,
    };
    #[tokio::test]
    async fn eose_closes_subscription_incomplete_and_stalled_relays_fail() {
        for complete in [true, false] {
            let (a, b) = tokio::io::duplex(4096);
            let mut client = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
            let mut server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
            let task = tokio::spawn(async move {
                let req = server.next().await.unwrap().unwrap().into_text().unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&req).unwrap()[0],
                    "REQ"
                );
                server
                    .send(Message::Text(r#"["EVENT","other",{}]"#.into()))
                    .await
                    .unwrap();
                server
                    .send(Message::Text(
                        r#"["EVENT","pinakes",{"id":"fixture"}]"#.into(),
                    ))
                    .await
                    .unwrap();
                server
                    .send(Message::Text(
                        if complete {
                            r#"["EOSE","pinakes"]"#
                        } else {
                            r#"["CLOSED","pinakes","denied"]"#
                        }
                        .into(),
                    ))
                    .await
                    .unwrap();
                let close = server.next().await.unwrap().unwrap().into_text().unwrap();
                assert_eq!(close, r#"["CLOSE","pinakes"]"#);
            });
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                nostr_subscription(&mut client, serde_json::json!({"kinds":[31923]})),
            )
            .await
            .unwrap();
            if complete {
                assert_eq!(result.unwrap().len(), 1)
            } else {
                assert!(result.is_err())
            }
            task.await.unwrap();
        }
        let (a, _b) = tokio::io::duplex(4096);
        let mut socket = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            nostr_subscription(&mut socket, serde_json::json!({}))
        )
        .await
        .is_err());
    }
}
