//! The HTTP client every catalogue importer asks its upstreams with ([`client`], [`Client`]), the
//! one rule for a URL an operator names with `pinakesd catalogue import --from` ([`from_url`]),
//! and what the catalogues of bulk files share: where a file comes from ([`Input`]: the official
//! URL, another URL, or a local file), the version a file's answer names without reading it
//! ([`Input::version`]), and the bridge from a blocking reader on its own thread to the run's
//! writes ([`batches`]).
//!
//! The URLs are the operator's or the build's, never a reference's: no request here is steered
//! by what people share, so the client is not the egress. It asks directly (proxies named in the
//! environment are ignored), over `https` only (plain `http` only to a loopback address, the
//! tests' local servers), follows a redirect only to a host its catalogue names, names the
//! operator's contact in its User-Agent, waits as a `Retry-After` asks (a host that asks for
//! longer than [`LONGEST_WAIT`] is left alone until then: `rate_limited`), and every body is read
//! within a size cap and a deadline ([`Client::small`], `bulk::Download`).
use super::{bulk, failure::Failure, Card};
use anyhow::Result;
use std::{io::Read, path::PathBuf, time::Duration};

/// The most redirects one request follows (a mirror's own CDN host is one).
const REDIRECTS: usize = 3;
/// The longest wait for an answer's headers, or one chunk of a small answer.
pub const WAIT: Duration = Duration::from_secs(120);
/// The longest a small answer may take, all of it.
const SMALL_DEADLINE: Duration = Duration::from_secs(600);
/// How often one request is tried (on a connection error, a rate limit or a server error).
const TRIES: u32 = 6;
/// The first wait before a request is tried again (it doubles each time).
const FIRST_WAIT: Duration = Duration::from_secs(2);
/// The longest a `Retry-After` is waited for; one asking longer fails the run (`rate_limited`),
/// and the runner comes back later.
pub const LONGEST_WAIT: Duration = Duration::from_secs(300);
/// The longest a host's ask is kept: a day (a `Retry-After` of years would hold the importer off
/// until the daemon restarts).
const LONGEST_HOLD: Duration = Duration::from_secs(86_400);

/// Whether `host` is a loopback address (`127.0.0.0/8`, `::1`) or `localhost`.
fn loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}
/// Whether the client may ask `url`: `https`, or `http` on a loopback address; never with a user
/// name or password.
pub fn allowed(url: &url::Url) -> bool {
    let scheme = url.scheme() == "https" || (url.scheme() == "http" && loopback(url));
    scheme && url.username().is_empty() && url.password().is_none() && url.host().is_some()
}
/// Whether a redirect may go to `url`: one of `hosts` by its exact name, over `https` on its
/// default port (a loopback host the list names, as the tests' servers are, on any).
pub(crate) fn may_follow(url: &url::Url, hosts: &[&str]) -> bool {
    let listed = url.host_str().is_some_and(|h| hosts.contains(&h));
    listed && allowed(url) && (loopback(url) || (url.scheme() == "https" && url.port().is_none()))
}

/// What `--from` names when it is a URL (it has a `scheme://`): an `https://` URL (`http://` only
/// on a loopback address, the tests' servers) without a user name, a password or a query (a
/// signed link's token: the URL is kept in the import's cursor and shown in the log); `None` for a
/// local path. A refused URL is never repeated in the error.
pub fn from_url(text: &str) -> Result<Option<url::Url>> {
    if !text.contains("://") {
        return Ok(None);
    }
    let url = url::Url::parse(text).map_err(|_| anyhow::anyhow!("--from: not a URL"))?;
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "--from: a URL with a user name or password is refused"
    );
    anyhow::ensure!(
        url.query().is_none(),
        "--from: a URL with a query is refused"
    );
    anyhow::ensure!(
        allowed(&url),
        "--from: a URL is read over https (http only on a loopback address)"
    );
    Ok(Some(url))
}

/// The HTTP client of the catalogue importers ([`client`]). Every request it makes is to a URL
/// [`allowed`] says it may ask; another fails `url` without a byte sent.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    first_wait: Duration,
}
/// The client a catalogue asks its upstreams with: the operator's contact in its User-Agent,
/// connection and read timeouts, no proxy, no compressed answers (a file's bytes are what its
/// publisher serves, its length known, and nothing is unpacked that the caps don't see), and
/// redirects followed (at most 3) only to `hosts` (a mirror's own CDN host): a redirect anywhere
/// else is not followed, and its answer is no file. With no hosts, no redirect is followed.
pub fn client(contact: &str, hosts: &'static [&'static str]) -> Result<Client> {
    build(contact, hosts, false)
}
/// [`client`] for a JSON API, whose answers may come compressed (gzip): their bodies are read
/// whole within a cap of the unpacked bytes.
pub fn api_client(contact: &str, hosts: &'static [&'static str]) -> Result<Client> {
    build(contact, hosts, true)
}
fn build(contact: &str, hosts: &'static [&'static str], gzip: bool) -> Result<Client> {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() < REDIRECTS && may_follow(attempt.url(), hosts) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let http = reqwest::Client::builder()
        .no_proxy()
        .gzip(gzip)
        .no_brotli()
        .no_deflate()
        .redirect(policy)
        .user_agent(crate::resolver::egress::user_agent(contact))
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(WAIT)
        .build()?;
    Ok(Client {
        http,
        first_wait: FIRST_WAIT,
    })
}

/// The egress's turn for a host, held for one HTTP exchange ([`Client::send`]).
pub type Turn = Option<tokio::sync::OwnedMutexGuard<tokio::time::Instant>>;

/// What a HEAD of a file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// Its `Content-Length`, when it names one.
    pub size: Option<u64>,
    /// The validator `If-Range` can take ([`validator_of`]); empty when it has none.
    pub validator: String,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}
/// The validator `If-Range` can take: a strong `ETag`, else the `Last-Modified` date; empty
/// with neither. A weak `ETag` (`W/"…"`) never holds for a range, so every reconnect would read
/// as a file replaced.
pub fn validator_of(headers: &reqwest::header::HeaderMap) -> String {
    let text = |h: reqwest::header::HeaderName| headers.get(h)?.to_str().ok().map(str::to_owned);
    text(reqwest::header::ETAG)
        .filter(|t| !t.starts_with("W/"))
        .or_else(|| text(reqwest::header::LAST_MODIFIED))
        .unwrap_or_default()
}

/// The host of a URL, for a failure's `at`.
pub fn host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown host".into())
}
fn failure(class: &'static str, url: &str) -> Failure {
    Failure::new(class).at(host(url))
}

impl Client {
    /// The first wait before a request is tried again (it doubles each time; 2 s by default).
    pub fn first_wait(mut self, wait: Duration) -> Self {
        self.first_wait = wait;
        self
    }
    fn request(&self, method: reqwest::Method, url: &str) -> Result<reqwest::RequestBuilder> {
        let parsed = url::Url::parse(url).map_err(|_| failure("url", url))?;
        if !allowed(&parsed) {
            return Err(failure("url", url).into());
        }
        Ok(self.http.request(method, parsed))
    }
    /// A GET of `url` with `headers`, tried again on a connection error, a rate limit or a server
    /// error ([`TRIES`] times, waiting as `Retry-After` says, else 2, 4, 8, … s from the client's
    /// first wait): the answer of the last try otherwise. A host that asked to be left alone
    /// longer than [`LONGEST_WAIT`] is not asked before that time (`rate_limited`). With an
    /// egress, each try takes its turn for the host first: held for that exchange (the answer
    /// comes with it, to be held while its body is read), dropped before any wait.
    pub async fn send(
        &self,
        url: &str,
        headers: &[(reqwest::header::HeaderName, String)],
        egress: Option<&crate::resolver::egress::Egress>,
    ) -> Result<(reqwest::Response, Turn)> {
        self.send_with(reqwest::Method::GET, url, headers, egress, self.first_wait)
            .await
    }
    /// [`Client::send`] with a method and a first wait of its own.
    pub async fn send_with(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: &[(reqwest::header::HeaderName, String)],
        egress: Option<&crate::resolver::egress::Egress>,
        first_wait: Duration,
    ) -> Result<(reqwest::Response, Turn)> {
        let mut tries = 0;
        loop {
            tries += 1;
            if too_early(url) {
                return Err(failure("rate_limited", url).into());
            }
            let turn = egress_turn(egress, url).await;
            let mut request = self.request(method.clone(), url)?;
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let backoff = backoff(first_wait, tries);
            match tokio::time::timeout(WAIT, request.send()).await {
                Ok(Ok(response)) if transient(response.status()) && tries < TRIES => {
                    drop(turn);
                    // Never at once again, whatever `Retry-After` says.
                    let wait = retry_after(&response)
                        .unwrap_or(backoff)
                        .max(backoff.min(Duration::from_secs(1)));
                    if wait > LONGEST_WAIT {
                        hold(url, wait);
                        return Err(failure("rate_limited", url).into());
                    }
                    tracing::info!(host = %crate::resolver::log_host(url), status = response.status().as_u16(), wait_secs = wait.as_secs(), "catalogue request refused for now; trying again");
                    tokio::time::sleep(wait).await;
                }
                Ok(Ok(response)) => return Ok((response, turn)),
                Ok(Err(_)) | Err(_) if tries < TRIES => {
                    drop(turn);
                    tokio::time::sleep(backoff).await
                }
                Ok(Err(error)) => {
                    let class = if error.is_timeout() {
                        "timeout"
                    } else {
                        "connect"
                    };
                    return Err(failure(class, url).into());
                }
                Err(_) => return Err(failure("timeout", url).into()),
            }
        }
    }
    /// A small answer read whole: at most `cap` bytes within [`SMALL_DEADLINE`] (larger: `size`);
    /// none for a 404, a failure at the host for another answer than a success.
    pub async fn small(&self, url: &str, cap: usize) -> Result<Option<Vec<u8>>> {
        let read = async {
            let (response, _) = self.send(url, &[], None).await?;
            small_body(response, cap, url).await
        };
        tokio::time::timeout(SMALL_DEADLINE, read)
            .await
            .unwrap_or_else(|_| Err(failure("timeout", url).into()))
    }
    /// A HEAD of `url` (tried again as [`Client::send`] does), or `None` for a 404.
    pub async fn head(&self, url: &str) -> Result<Option<Head>> {
        let (response, _) = self
            .send_with(reqwest::Method::HEAD, url, &[], None, self.first_wait)
            .await?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => {
                // `content_length()` is 0 for every HEAD (no body follows): the header says it.
                let size = response
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let modified = response
                    .headers()
                    .get(reqwest::header::LAST_MODIFIED)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| httpdate::parse_http_date(v).ok())
                    .map(chrono::DateTime::<chrono::Utc>::from);
                Ok(Some(Head {
                    size,
                    validator: validator_of(response.headers()),
                    modified,
                }))
            }
            _ => Err(failure("download", url).into()),
        }
    }
}
/// An answer's body read whole, at most `cap` bytes (larger: `size`); none for a 404, a failure
/// at the host for another answer than a success.
pub async fn small_body(
    response: reqwest::Response,
    cap: usize,
    url: &str,
) -> Result<Option<Vec<u8>>> {
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(failure("download", url).into());
    }
    if response.content_length().is_some_and(|n| n > cap as u64) {
        return Err(failure("size", url).into());
    }
    let mut out = Vec::new();
    let mut response = response;
    loop {
        match tokio::time::timeout(WAIT, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                if out.len() + chunk.len() > cap {
                    return Err(failure("size", url).into());
                }
                out.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => return Ok(Some(out)),
            _ => return Err(failure("download", url).into()),
        }
    }
}

/// When a host (with its port) asked to be left alone longer than [`LONGEST_WAIT`]: until then
/// no request goes to it, and a run that would ask fails `rate_limited` at once.
static NOT_BEFORE: std::sync::Mutex<std::collections::BTreeMap<String, std::time::Instant>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());
fn authority(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    Some(format!(
        "{}:{}",
        url.host_str()?,
        url.port_or_known_default()?
    ))
}
/// `url`'s host asked not to be asked for `wait` (at most [`LONGEST_HOLD`]).
pub fn hold(url: &str, wait: Duration) {
    let Some(host) = authority(url) else {
        return;
    };
    if let Some(until) = std::time::Instant::now().checked_add(wait.min(LONGEST_HOLD)) {
        let mut hosts = NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner());
        hosts.insert(host, until);
    }
}
/// Forget what `url`'s host asked.
#[cfg(test)]
pub(crate) fn forget_hold(url: &str) {
    if let Some(host) = authority(url) {
        let mut hosts = NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner());
        hosts.remove(&host);
    }
}
/// Whether `url`'s host asked to be left alone for longer than now.
pub fn too_early(url: &str) -> bool {
    let Some(host) = authority(url) else {
        return false;
    };
    let hosts = NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner());
    hosts
        .get(&host)
        .is_some_and(|t| std::time::Instant::now() < *t)
}
/// How long a refusal asks to wait (`Retry-After`, in seconds or as a date).
pub fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    wait_of(
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?,
    )
}
/// A `Retry-After` value as a wait from now: seconds, or an HTTP date (none past it).
fn wait_of(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    )
}
/// The wait before try `n + 1` (from 1): `first`, twice it, four times, …, at most
/// [`LONGEST_WAIT`].
pub fn backoff(first: Duration, n: u32) -> Duration {
    first
        .saturating_mul(2u32.saturating_pow(n.clamp(1, 9) - 1))
        .min(LONGEST_WAIT)
}
/// Whether an answer is worth trying again: a rate limit or a server error.
fn transient(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}
/// The egress's turn for `url`'s host, waiting for it as long as it takes (a queue too long for
/// the egress's own wait is waited out); none without an egress.
pub(crate) async fn egress_turn(
    egress: Option<&crate::resolver::egress::Egress>,
    url: &str,
) -> Turn {
    let egress = egress?;
    let host = url::Url::parse(url).ok()?.host_str()?.to_owned();
    loop {
        match egress.host_turn(&host).await {
            Ok(turn) => return Some(turn),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// Where a catalogue file is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Url(url::Url),
    /// A local file (`--from FILE`).
    File(PathBuf),
}
impl Input {
    /// A URL ([`from_url`]'s rule) or a local path.
    pub fn parse(from: &str) -> Result<Self> {
        Ok(match from_url(from)? {
            Some(url) => Input::Url(url),
            None => Input::File(PathBuf::from(from)),
        })
    }
    /// The URL `path` names relative to this one (F-Droid's index next to its entry file); a
    /// local file's sibling for a file.
    pub fn join(&self, path: &str) -> Result<Self> {
        match self {
            Input::Url(url) => Ok(Input::Url(
                url.join(path.trim_start_matches('/'))
                    .map_err(|_| Failure::new("download"))?,
            )),
            Input::File(file) => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .ok_or_else(|| Failure::new("download"))?;
                Ok(Input::File(file.with_file_name(name)))
            }
        }
    }
    /// The host or path a failure names (`Failure::at`).
    pub fn at(&self) -> String {
        match self {
            Input::Url(url) => url.host_str().unwrap_or_default().to_owned(),
            Input::File(file) => file.display().to_string(),
        }
    }
    /// The version of the file as its answer names it, without reading the body: a strong ETag,
    /// else Last-Modified (`HEAD`); for a local file, its size and modification time. With the
    /// time it names, when it names one.
    pub async fn version(
        &self,
        client: &Client,
    ) -> Result<(String, Option<chrono::DateTime<chrono::Utc>>)> {
        match self {
            Input::Url(url) => {
                let head = client
                    .head(url.as_str())
                    .await?
                    .ok_or_else(|| Failure::new("not_found").at(self.at()))?;
                if head.validator.is_empty() {
                    anyhow::bail!(Failure::new("version").at(self.at()));
                }
                let mut version = head.validator;
                crate::model::truncate(&mut version, 200);
                Ok((version, head.modified))
            }
            Input::File(file) => {
                let meta =
                    std::fs::metadata(file).map_err(|_| Failure::new("not_found").at(self.at()))?;
                let at = meta
                    .modified()
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from);
                Ok((
                    format!(
                        "{}:{}",
                        meta.len(),
                        at.map(|a| a.timestamp()).unwrap_or_default()
                    ),
                    at,
                ))
            }
        }
    }
    /// The file's body as a blocking reader, at most `max_bytes` (call it on a blocking
    /// thread: a download reads through `handle`).
    pub fn open(
        &self,
        client: &Client,
        handle: &tokio::runtime::Handle,
        max_bytes: u64,
    ) -> Result<Box<dyn Read + Send>> {
        match self {
            Input::Url(url) => Ok(Box::new(bulk::Download::open(
                client.clone(),
                handle.clone(),
                url.clone(),
                max_bytes,
                RETRY_WAIT,
            )?)),
            Input::File(file) => {
                let opened = std::fs::File::open(file)
                    .map_err(|_| Failure::new("not_found").at(self.at()))?;
                Ok(Box::new(opened.take(max_bytes)))
            }
        }
    }
    /// The whole body, at most `max_bytes` (a small file: an index's entry, a manifest).
    pub async fn read_small(&self, client: &Client, max_bytes: u64) -> Result<Vec<u8>> {
        match self {
            Input::Url(url) => {
                let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
                client
                    .small(url.as_str(), cap)
                    .await?
                    .ok_or_else(|| Failure::new("not_found").at(self.at()).into())
            }
            Input::File(file) => {
                let opened = std::fs::File::open(file)
                    .map_err(|_| Failure::new("not_found").at(self.at()))?;
                let mut body = Vec::new();
                opened.take(max_bytes + 1).read_to_end(&mut body)?;
                if body.len() as u64 > max_bytes {
                    anyhow::bail!(Failure::new("size").at(self.at()));
                }
                Ok(body)
            }
        }
    }
}
/// The first wait before a broken download is asked for again (it doubles each time).
const RETRY_WAIT: Duration = Duration::from_secs(5);

/// The cards a blocking `read` gives, in parts, as they come: `read` runs on a blocking thread
/// and hands each part to its sink, which says whether to go on (false once the receiver is
/// gone: the run stopped). Its failure comes as the last item; a panic is a failure (`panic`).
pub fn batches<F>(read: F) -> tokio::sync::mpsc::Receiver<Result<Vec<Card>>>
where
    F: FnOnce(&mut dyn FnMut(Vec<Card>) -> bool) -> Result<()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let sent = tx.clone();
        let mut sink = move |part: Vec<Card>| sent.blocking_send(Ok(part)).is_ok();
        let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read(&mut sink)));
        let failed = match read {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some(Failure::new("panic").into()),
        };
        if let Some(error) = failed {
            let _ = tx.blocking_send(Err(error));
        }
    });
    rx
}

/// The cards of one part a reader hands over at a time.
pub const PART: usize = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    /// With the daemon's egress, an API request waits for the egress's turn for the host: not
    /// while one of the egress's own requests to it is under way.
    #[tokio::test]
    async fn the_api_takes_the_egress_turn_for_its_host() {
        // Never the network: recorded answers (no request is made).
        let (egress, _) = crate::sources::fixture::egress(vec![]);
        let url = "https://www.wikidata.org/w/api.php?action=wbgetentities";
        let held = egress.host_turn("www.wikidata.org").await.unwrap();
        let waiting =
            tokio::time::timeout(Duration::from_millis(300), egress_turn(Some(&egress), url));
        assert!(
            waiting.await.is_err(),
            "it waits while the egress's request runs"
        );
        drop(held);
        let turn = tokio::time::timeout(Duration::from_secs(30), egress_turn(Some(&egress), url));
        assert!(turn.await.unwrap().is_some());
        assert!(egress_turn(None, url).await.is_none());
    }

    /// A host that asked to be left alone longer than five minutes is not asked again before.
    #[test]
    fn a_long_wait_asked_holds_every_request_to_the_host() {
        let url = "https://example.org:4443/w/api.php";
        assert!(!too_early(url));
        hold(url, Duration::from_secs(3600));
        assert!(too_early(url) && too_early("https://example.org:4443/other"));
        assert!(!too_early("https://example.org/w/api.php"));
        forget_hold(url);
        assert!(!too_early(url));
        // Years are held a day at most, and never overflow.
        hold(url, Duration::from_secs(u64::MAX));
        assert!(too_early(url));
        let until = NOT_BEFORE.lock().unwrap()["example.org:4443"];
        assert!(until <= std::time::Instant::now() + LONGEST_HOLD);
        forget_hold(url);
    }

    #[test]
    fn a_weak_etag_is_no_validator() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::ETAG, "\"strong\"".parse().unwrap());
        assert_eq!(validator_of(&h), "\"strong\"");
        h.insert(reqwest::header::ETAG, "W/\"weak\"".parse().unwrap());
        assert_eq!(validator_of(&h), "");
        let date = "Wed, 25 Nov 2026 08:00:25 GMT";
        h.insert(reqwest::header::LAST_MODIFIED, date.parse().unwrap());
        assert_eq!(validator_of(&h), date);
    }

    #[test]
    fn a_retry_after_is_read_in_seconds_or_as_a_date() {
        assert_eq!(wait_of(" 120 "), Some(Duration::from_secs(120)));
        let soon = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(90));
        let wait = wait_of(&soon).unwrap();
        assert!(
            wait > Duration::from_secs(80) && wait <= Duration::from_secs(90),
            "{wait:?}"
        );
        let past = httpdate::fmt_http_date(std::time::SystemTime::now() - Duration::from_secs(90));
        assert_eq!(wait_of(&past), Some(Duration::ZERO));
        assert_eq!(wait_of("soon"), None);
    }

    #[test]
    fn inputs_are_urls_or_files() {
        assert_eq!(
            Input::parse("https://f-droid.org/repo/entry.json").unwrap(),
            Input::Url(url::Url::parse("https://f-droid.org/repo/entry.json").unwrap())
        );
        assert_eq!(
            Input::parse("/tmp/x.json").unwrap(),
            Input::File("/tmp/x.json".into())
        );
        let entry = Input::parse("https://f-droid.org/repo/entry.json").unwrap();
        assert_eq!(
            entry.join("/index-v2.json").unwrap(),
            Input::Url(url::Url::parse("https://f-droid.org/repo/index-v2.json").unwrap())
        );
        // A file's sibling never leaves its directory.
        assert_eq!(
            Input::File("/data/entry.json".into())
                .join("../../etc/passwd")
                .unwrap(),
            Input::File("/data/passwd".into())
        );
    }

    /// The bulk client follows a redirect only to an https URL on a host its catalogue names; a
    /// body is read within its cap.
    #[tokio::test]
    async fn redirects_go_only_to_named_hosts_and_bodies_stay_within_their_cap() {
        use axum::{response::Redirect, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        // A redirect to a host the catalogue does not name (here the same server by another
        // name, which would answer): never followed.
        let elsewhere = format!("http://localhost:{port}/small");
        let app = Router::new()
            .route(
                "/away",
                get(move || {
                    let to = elsewhere.clone();
                    async move { Redirect::temporary(&to) }
                }),
            )
            .route("/small", get(|| async { "followed" }))
            // What the client says it accepts, in a chunked answer (no length).
            .route(
                "/encoding",
                get(|headers: axum::http::HeaderMap| async move {
                    let said = headers
                        .get("accept-encoding")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("none")
                        .to_owned();
                    axum::body::Body::from_stream(futures_util::stream::iter([Ok::<
                        _,
                        std::io::Error,
                    >(
                        said
                    )]))
                }),
            )
            .route("/big", get(|| async { vec![b'x'; 10_000] }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = client("test@example.org", &["static.crates.io"]).unwrap();
        let away = Input::parse(&format!("{base}/away")).unwrap();
        let failed = away.read_small(&client, 1 << 20).await.unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "download");
        // No compressed answer is asked for (a gzip answer has no length a download can check),
        // and a small file without a length reads.
        let encoding = Input::parse(&format!("{base}/encoding")).unwrap();
        assert_eq!(encoding.read_small(&client, 1000).await.unwrap(), b"none");
        let big = Input::parse(&format!("{base}/big")).unwrap();
        let failed = big.read_small(&client, 1000).await.unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "size");
        assert_eq!(big.read_small(&client, 20_000).await.unwrap().len(), 10_000);
        let handle = tokio::runtime::Handle::current();
        // A download over its cap fails, at its answer's length or while it is read.
        let read = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            big.open(&client, &handle, 1000)
                .and_then(|mut body| Ok(body.read_to_end(&mut out)?))
        })
        .await
        .unwrap();
        assert!(read.is_err(), "{read:?}");
        // A URL with a credential is refused without being repeated.
        let refused = Input::parse("https://user:secret@example.org/x")
            .unwrap_err()
            .to_string();
        assert!(!refused.contains("secret"), "{refused}");
    }

    #[tokio::test]
    async fn a_failing_reader_ends_its_parts_with_its_failure() {
        let mut parts = batches(|sink| {
            assert!(sink(vec![Card::default()]));
            anyhow::bail!(Failure::new("archive"))
        });
        assert_eq!(parts.recv().await.unwrap().unwrap().len(), 1);
        let failed = parts.recv().await.unwrap().unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "archive");
        assert!(parts.recv().await.is_none());
        let mut parts = batches(|_| panic!("a reader's bug"));
        let failed = parts.recv().await.unwrap().unwrap_err();
        assert_eq!(failed.downcast_ref::<Failure>().unwrap().class, "panic");
    }
}
