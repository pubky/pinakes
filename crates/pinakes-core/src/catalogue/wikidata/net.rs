//! What the Wikidata importer asks of Wikimedia: the dump directory, the dump itself (ranges of
//! it, on two connections, or as a stream), its checksums, the recent changes and the changed
//! items.
//!
//! Every request goes to a fixed host (`dumps.wikimedia.org`, `www.wikidata.org`, or the URL an
//! operator names with `pinakesd catalogue import --from`), with Pinakes' User-Agent and the
//! operator's contact, one at a time per kind (the dump's two connections are the one exception
//! Wikimedia allows), no redirects, and bounded answers. A failure is a [`Failure`]: its class and
//! the host, never what an answer said.
use crate::catalogue::failure::Failure;
use anyhow::{Context as _, Result};
use serde_json::Value;
use std::time::Duration;

/// Where Wikidata's weekly dumps are listed.
pub const DUMPS: &str = "https://dumps.wikimedia.org/wikidatawiki/entities/";
/// Wikidata's API.
pub const API: &str = "https://www.wikidata.org/w/api.php";
/// The most dated directories looked at for the newest full dump.
const DIRECTORIES: usize = 6;
/// The largest answer read from the API (a `wbgetentities` of 50 large items is a few MB).
pub const API_CAP: usize = 64 << 20;
/// The largest directory page or checksum file read.
const PAGE_CAP: usize = 4 << 20;
/// How often one request is tried before the run fails.
const TRIES: u32 = 6;
/// The longest wait a `Retry-After` is followed for; one asking longer fails the run
/// (`rate_limited`), and the runner comes back later.
const LONGEST_WAIT: Duration = Duration::from_secs(300);
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
/// The longest a host's ask is kept: a day (a `Retry-After` of years would hold the importer off
/// until the daemon restarts).
const LONGEST_HOLD: Duration = Duration::from_secs(86_400);
/// `url`'s host asked not to be asked for `wait` (at most [`LONGEST_HOLD`]).
fn wait_asked(url: &str, wait: Duration) {
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
fn forget_wait_asked(url: &str) {
    if let Some(host) = authority(url) {
        let mut hosts = NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner());
        hosts.remove(&host);
    }
}
/// Whether `url`'s host asked to be left alone for longer than now.
fn too_early(url: &str) -> bool {
    let Some(host) = authority(url) else {
        return false;
    };
    let hosts = NOT_BEFORE.lock().unwrap_or_else(|e| e.into_inner());
    hosts
        .get(&host)
        .is_some_and(|t| std::time::Instant::now() < *t)
}
/// Bytes a read must bring in [`SLOW_WINDOW`] of waiting for them: a server slower than that (a
/// few bytes every two minutes) would hold the import, and the large-import turn, without end.
const SLOW_BYTES: u64 = 8 << 20;
/// The time a dump read may wait for [`SLOW_BYTES`] (`Wikidata::slow_window`).
pub const SLOW_WINDOW: Duration = Duration::from_secs(600);
/// A read's speed floor, counting only the time spent waiting for the server's bytes (not the
/// reader's own pauses: a slow batch write is no slow server).
struct Speed {
    window: Duration,
    waited: Duration,
    bytes: u64,
}
impl Speed {
    fn new(window: Duration) -> Self {
        Self {
            window,
            waited: Duration::ZERO,
            bytes: 0,
        }
    }
    /// `n` bytes more came after waiting `waited` for them; false once the window of waiting
    /// passed without [`SLOW_BYTES`].
    fn read(&mut self, n: u64, waited: Duration) -> bool {
        self.waited += waited;
        self.bytes += n;
        if self.bytes >= SLOW_BYTES {
            self.waited = Duration::ZERO;
            self.bytes = 0;
            return true;
        }
        self.waited <= self.window
    }
}
/// The longest a page, a checksum file or an API answer may take, all of it.
const ANSWER_DEADLINE: Duration = Duration::from_secs(600);
/// The least time between two API requests of the importer (short answers never go back to
/// back). In the daemon they also take the egress's turn for `www.wikidata.org`
/// ([`share_egress_pace`]): one request at a time with the resolver's, at its 200 a minute.
const API_INTERVAL: Duration = Duration::from_millis(300);
/// Bytes a stream must read after a break before its tries count from zero again.
const PROGRESS: u64 = 8 << 20;

/// The HTTP client of the importer: the User-Agent with the operator's contact, no redirects.
/// HTTPS only, unless `http` (the tests' local server).
pub fn client(contact: &str, gzip: bool, http: bool) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(crate::resolver::egress::user_agent(contact))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(!http)
        // Fixed public hosts, asked directly: no proxy from the environment.
        .no_proxy()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .gzip(gzip)
        .build()?)
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

/// One weekly dump: its date (`YYYYMMDD`), the URL of its JSON file, its size and validator.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dump {
    pub date: String,
    pub url: String,
    pub size: u64,
    /// The `ETag` (or `Last-Modified`) the file was first read with: a file that changed under
    /// a resumed read is read again from its start.
    pub validator: String,
}
impl Dump {
    /// The dump's date as a time: its day's start (UTC). The dump holds the items as they were
    /// while it was written, from that day on, so changes are read again from then.
    pub fn time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let day = chrono::NaiveDate::parse_from_str(&self.date, "%Y%m%d").ok()?;
        Some(day.and_hms_opt(0, 0, 0)?.and_utc())
    }
    /// The dump's checksum file (`wikidata-<date>-sha1sums.txt` beside it).
    pub fn sums_url(&self) -> Option<String> {
        let (dir, _) = self.url.rsplit_once('/')?;
        Some(format!("{dir}/wikidata-{}-sha1sums.txt", self.date))
    }
    /// The file's name.
    pub fn file_name(&self) -> String {
        format!("wikidata-{}-all.json.bz2", self.date)
    }
}

/// A body read whole, at most `cap` bytes.
async fn body(response: reqwest::Response, cap: usize, url: &str) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > cap as u64) {
        return Err(failure("size", url).into());
    }
    let mut out = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failure("download", url))?
    {
        if out.len() + chunk.len() > cap {
            return Err(failure("size", url).into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// How long a refusal asks to wait (`Retry-After`, in seconds or as a date).
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
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
/// The wait before try `n` (from 1): 2, 4, 8, … seconds, at most [`LONGEST_WAIT`].
fn backoff(n: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(n.min(8))).min(LONGEST_WAIT)
}
/// Whether an answer is worth trying again: a rate limit or a server error.
fn transient(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// A GET that is tried again on a connection error, a rate limit or a server error (waiting as
/// `Retry-After` says), with `headers`; the answer of the last try otherwise.
async fn get(
    client: &reqwest::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, String)],
) -> Result<reqwest::Response> {
    Ok(get_paced(client, url, headers, None).await?.0)
}
/// The egress's turn for a host, held for one HTTP exchange ([`get_paced`]).
type Turn = Option<tokio::sync::OwnedMutexGuard<tokio::time::Instant>>;
/// [`get`], each try taking `egress`'s turn for the host first: held for that exchange (the
/// answer comes with it, to be held while its body is read), dropped before any wait, so the
/// egress's own requests to the host go between the tries.
async fn get_paced(
    client: &reqwest::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, String)],
    egress: Option<&crate::resolver::egress::Egress>,
) -> Result<(reqwest::Response, Turn)> {
    let mut tries = 0;
    loop {
        tries += 1;
        if too_early(url) {
            return Err(failure("rate_limited", url).into());
        }
        let turn = egress_turn(egress, url).await;
        let mut request = client.get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        match request.send().await {
            Ok(response) if transient(response.status()) && tries < TRIES => {
                drop(turn);
                // Never at once again, whatever `Retry-After` says.
                let wait = retry_after(&response)
                    .unwrap_or_else(|| backoff(tries))
                    .max(Duration::from_secs(1));
                if wait > LONGEST_WAIT {
                    wait_asked(url, wait);
                    return Err(failure("rate_limited", url).into());
                }
                tracing::info!(host = %crate::resolver::log_host(url), status = response.status().as_u16(), wait_secs = wait.as_secs(), "catalogue request refused for now; trying again");
                tokio::time::sleep(wait).await;
            }
            Ok(response) => return Ok((response, turn)),
            Err(_) if tries < TRIES => {
                drop(turn);
                tokio::time::sleep(backoff(tries)).await
            }
            Err(error) => {
                let class = if error.is_timeout() {
                    "timeout"
                } else {
                    "connect"
                };
                return Err(failure(class, url).into());
            }
        }
    }
}

/// What a HEAD of a dump file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub size: u64,
    /// Its `ETag`, else its `Last-Modified`; empty when it has neither.
    pub validator: String,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}
/// The validator `If-Range` can take: a strong `ETag`, else the `Last-Modified` date; empty
/// with neither. A weak `ETag` (`W/"…"`) never holds for a range, so every reconnect would read
/// as a file replaced.
fn validator_of(headers: &reqwest::header::HeaderMap) -> String {
    let text = |h: reqwest::header::HeaderName| headers.get(h)?.to_str().ok().map(str::to_owned);
    text(reqwest::header::ETAG)
        .filter(|t| !t.starts_with("W/"))
        .or_else(|| text(reqwest::header::LAST_MODIFIED))
        .unwrap_or_default()
}
/// A HEAD of the dump file, or `None` when there is none (404).
pub async fn head(client: &reqwest::Client, url: &str) -> Result<Option<Head>> {
    let mut tries = 0;
    let response = loop {
        tries += 1;
        if too_early(url) {
            return Err(failure("rate_limited", url).into());
        }
        match client.head(url).send().await {
            Ok(r) if transient(r.status()) && tries < TRIES => {
                let wait = retry_after(&r).unwrap_or_else(|| backoff(tries));
                if wait > LONGEST_WAIT {
                    wait_asked(url, wait);
                    return Err(failure("rate_limited", url).into());
                }
                // At least a second, as `get` waits: a `Retry-After: 0` is no reason to ask at once.
                tokio::time::sleep(wait.max(Duration::from_secs(1))).await
            }
            Ok(r) => break r,
            Err(_) if tries < TRIES => tokio::time::sleep(backoff(tries)).await,
            Err(_) => return Err(failure("connect", url).into()),
        }
    };
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => Ok(None),
        status if status.is_success() => {
            // `content_length()` is 0 for every HEAD (no body follows): the header says the size.
            let size = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| failure("download", url))?;
            let validator = validator_of(response.headers());
            let modified = response
                .headers()
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| httpdate::parse_http_date(v).ok())
                .map(chrono::DateTime::<chrono::Utc>::from);
            Ok(Some(Head {
                size,
                validator,
                modified,
            }))
        }
        _ => Err(failure("download", url).into()),
    }
}

/// The dated directories a dumps index page lists (`20261123/`), newest first.
pub fn dated_directories(page: &str) -> Vec<String> {
    let mut dates: Vec<String> = page
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter_map(|href| href.strip_suffix('/'))
        .filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
        .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y%m%d").is_ok())
        .map(str::to_owned)
        .collect();
    dates.sort();
    dates.dedup();
    dates.reverse();
    dates
}

/// The newest dump whose full JSON file is published under `base`.
pub async fn latest_dump(client: &reqwest::Client, base: &str) -> Result<Dump> {
    let page = whole(client, base, PAGE_CAP)
        .await?
        .ok_or_else(|| failure("download", base))?;
    let page = String::from_utf8_lossy(&page);
    for date in dated_directories(&page).into_iter().take(DIRECTORIES) {
        let url = format!(
            "{}/{date}/wikidata-{date}-all.json.bz2",
            base.trim_end_matches('/')
        );
        if let Some(Head {
            size, validator, ..
        }) = head(client, &url).await?
        {
            return Ok(Dump {
                date,
                url,
                size,
                validator,
            });
        }
    }
    Err(failure("no_dump", base).into())
}

/// A whole answer of at most `cap` bytes within [`ANSWER_DEADLINE`]; none for a 404.
async fn whole(client: &reqwest::Client, url: &str, cap: usize) -> Result<Option<Vec<u8>>> {
    let read = async {
        let response = get(client, url, &[]).await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(failure("download", url).into());
        }
        body(response, cap, url).await.map(Some)
    };
    tokio::time::timeout(ANSWER_DEADLINE, read)
        .await
        .unwrap_or_else(|_| Err(failure("timeout", url).into()))
}

/// The SHA-1 a checksum file lists for `file`, if it lists it.
pub fn listed_sha1(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file
            && sum.len() == 40
            && sum.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| sum.to_ascii_lowercase())
    })
}
/// The SHA-1 the dump's checksum file lists for it; none when the file or the line is missing.
pub async fn dump_sha1(client: &reqwest::Client, dump: &Dump) -> Result<Option<String>> {
    let Some(url) = dump.sums_url() else {
        return Ok(None);
    };
    let Some(sums) = whole(client, &url, PAGE_CAP).await? else {
        return Ok(None);
    };
    Ok(listed_sha1(
        &String::from_utf8_lossy(&sums),
        &dump.file_name(),
    ))
}

/// Bytes `from..to` of the dump, written to `file` at their offset as they come; returns the
/// offset reached (`to` when whole). A broken connection is taken up again where it stopped,
/// and a file that changed (another validator) fails the run.
pub async fn read_range(
    client: &reqwest::Client,
    dump: &Dump,
    file: &std::path::Path,
    from: u64,
    to: u64,
    slow: Duration,
) -> Result<u64> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(file)
        .await
        .with_context(|| Failure::new("disk").at(file.display().to_string()))?;
    if dump.validator.is_empty() {
        // Without a validator a file replaced between two ranges would be spliced.
        return Err(failure("download", &dump.url).into());
    }
    let mut at = from;
    let mut tries = 0;
    while at < to {
        tries += 1;
        let range = format!("bytes={at}-{}", to - 1);
        let response = get(
            client,
            &dump.url,
            &[
                (reqwest::header::RANGE, range),
                (reqwest::header::IF_RANGE, dump.validator.clone()),
            ],
        )
        .await?;
        match response.status() {
            reqwest::StatusCode::PARTIAL_CONTENT => {}
            // `If-Range` did not hold: the file was replaced.
            reqwest::StatusCode::OK => return Err(failure("dump_changed", &dump.url).into()),
            reqwest::StatusCode::NOT_FOUND => return Err(failure("dump_gone", &dump.url).into()),
            _ => return Err(failure("download", &dump.url).into()),
        }
        if range_start(&response) != Some(at) {
            return Err(failure("download", &dump.url).into());
        }
        out.seek(std::io::SeekFrom::Start(at))
            .await
            .with_context(|| Failure::new("disk").at(file.display().to_string()))?;
        let mut response = response;
        let mut speed = Speed::new(slow);
        loop {
            let asked = std::time::Instant::now();
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let waited = asked.elapsed();
                    let room = (to - at) as usize;
                    let chunk = &chunk[..chunk.len().min(room)];
                    out.write_all(chunk)
                        .await
                        .with_context(|| Failure::new("disk").at(file.display().to_string()))?;
                    at += chunk.len() as u64;
                    if !speed.read(chunk.len() as u64, waited) {
                        // Too slow: the run fails, and the next try reads this round again from
                        // its start (the cursor keeps the rounds before it).
                        return Err(failure("download", &dump.url).into());
                    }
                    if at >= to {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        if at < to {
            if tries >= TRIES {
                return Err(failure("download", &dump.url).into());
            }
            tokio::time::sleep(backoff(tries)).await;
        } else {
            tries = 0;
        }
    }
    out.flush()
        .await
        .with_context(|| Failure::new("disk").at(file.display().to_string()))?;
    out.sync_data()
        .await
        .with_context(|| Failure::new("disk").at(file.display().to_string()))?;
    Ok(at)
}

/// The first byte a partial answer holds (its `Content-Range`).
fn range_start(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// The dump from byte `from` to its `size`, as a stream of chunks for a reading thread
/// ([`ChannelReader`]): read until the reader is dropped or the file's end. A broken connection
/// is taken up again where it stopped (`If-Range` on the validator); a failure is sent as an
/// error holding its [`Failure`].
pub fn stream(
    client: reqwest::Client,
    url: String,
    validator: String,
    from: u64,
    size: u64,
    slow: Duration,
) -> ChannelReader {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(64);
    tokio::spawn(async move {
        let fail = |class: &'static str| std::io::Error::other(failure(class, &url));
        if validator.is_empty() {
            // Without a validator a broken connection taken up again could splice two files.
            let _ = tx.send(Err(fail("download"))).await;
            return;
        }
        let mut at = from;
        let mut tries = 0;
        // Where the tries were counted from zero last.
        let mut counted = from;
        while at < size {
            tries += 1;
            let headers = [
                (reqwest::header::RANGE, format!("bytes={at}-")),
                (reqwest::header::IF_RANGE, validator.clone()),
            ];
            let response = match get(&client, &url, &headers).await {
                Ok(r) => r,
                Err(error) => {
                    let error = error
                        .downcast::<Failure>()
                        .unwrap_or_else(|_| failure("download", &url));
                    let _ = tx.send(Err(std::io::Error::other(error))).await;
                    return;
                }
            };
            match response.status() {
                // Only the bytes asked for: another offset would splice the file.
                reqwest::StatusCode::PARTIAL_CONTENT if range_start(&response) == Some(at) => {}
                // A server that ignores ranges answers the whole file: fine from the start.
                reqwest::StatusCode::OK if at == 0 => {}
                reqwest::StatusCode::OK => {
                    let _ = tx.send(Err(fail("dump_changed"))).await;
                    return;
                }
                reqwest::StatusCode::NOT_FOUND => {
                    let _ = tx.send(Err(fail("dump_gone"))).await;
                    return;
                }
                _ => {
                    let _ = tx.send(Err(fail("download"))).await;
                    return;
                }
            }
            let mut response = response;
            let mut speed = Speed::new(slow);
            // A chunk read; the end of the answer or a break ends the loop. Only the time
            // waiting for the server counts toward the speed floor, not the reader's pauses.
            let mut asked = std::time::Instant::now();
            while let Ok(Some(chunk)) = response.chunk().await {
                let waited = asked.elapsed();
                let room = (size - at) as usize;
                let chunk = chunk.slice(..chunk.len().min(room));
                at += chunk.len() as u64;
                if !speed.read(chunk.len() as u64, waited) {
                    // Too slow to ever end: the run fails, and resumes from its cursor later.
                    let _ = tx.send(Err(fail("download"))).await;
                    return;
                }
                if at - counted >= PROGRESS {
                    tries = 0;
                    counted = at;
                }
                if tx.send(Ok(chunk)).await.is_err() {
                    return;
                }
                if at >= size {
                    return;
                }
                asked = std::time::Instant::now();
            }
            if at >= size {
                return;
            }
            if tries >= TRIES {
                let _ = tx.send(Err(fail("download"))).await;
                return;
            }
            tokio::time::sleep(backoff(tries)).await;
        }
    });
    ChannelReader {
        rx,
        chunk: bytes::Bytes::new(),
    }
}

/// The run's failure a read error holds (a stream's), if any.
pub fn failure_in(error: &std::io::Error) -> Option<Failure> {
    error.get_ref()?.downcast_ref::<Failure>().cloned()
}

/// A `Read` over chunks a task sends; for a reading thread, never inside the runtime.
pub struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>,
    chunk: bytes::Bytes,
}
impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.chunk.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => self.chunk = chunk,
                Some(Err(error)) => return Err(error),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len());
        buf[..n].copy_from_slice(&self.chunk[..n]);
        let _ = self.chunk.split_to(n);
        Ok(n)
    }
}

/// An API answer as JSON: a rate limit, a server error and Wikimedia's `maxlag` refusal are
/// waited out and tried again; another error fails the run (`api`).
pub async fn api(client: &reqwest::Client, base: &str, query: &[(&str, &str)]) -> Result<Value> {
    api_capped(client, base, query, API_CAP).await
}
/// [`api`], with answers of at most `cap` bytes (larger: `size`).
pub async fn api_capped(
    client: &reqwest::Client,
    base: &str,
    query: &[(&str, &str)],
    cap: usize,
) -> Result<Value> {
    let mut url = url::Url::parse(base).map_err(|_| failure("api", base))?;
    url.query_pairs_mut()
        .extend_pairs(query)
        .append_pair("format", "json")
        .append_pair("formatversion", "2")
        .append_pair("maxlag", "5");
    let url = url.to_string();
    let mut tries = 0;
    loop {
        tries += 1;
        pace().await;
        let read = async {
            // The egress's turn is held for the exchange and its body, never for a wait.
            let (response, turn) = get_paced(client, &url, &[], EGRESS.get()).await?;
            let status = response.status();
            let wait = retry_after(&response);
            if !status.is_success() {
                return Err(anyhow::Error::new(failure("api", &url)));
            }
            let raw = body(response, cap, &url).await?;
            drop(turn);
            Ok((wait, raw))
        };
        let (wait, raw) = tokio::time::timeout(ANSWER_DEADLINE, read)
            .await
            .unwrap_or_else(|_| Err(failure("timeout", &url).into()))?;
        let value: Value = serde_json::from_slice(&raw).map_err(|_| failure("api", &url))?;
        if value["error"]["code"] == "maxlag" {
            let wait = wait.unwrap_or(Duration::from_secs(5));
            // Wikidata still lagging after the tries: a wait it asks for, not a fault.
            if wait > LONGEST_WAIT || tries >= TRIES {
                if wait > LONGEST_WAIT {
                    wait_asked(&url, wait);
                }
                return Err(failure("rate_limited", &url).into());
            }
            tracing::info!(host = %crate::resolver::log_host(&url), wait_secs = wait.as_secs(), "Wikidata asks to wait (maxlag); trying again");
            tokio::time::sleep(wait).await;
            continue;
        }
        if value.get("error").is_some() {
            return Err(failure("api", &url).into());
        }
        return Ok(value);
    }
}

/// The daemon's egress ([`share_egress_pace`]): the API requests take its turn for their host.
static EGRESS: std::sync::OnceLock<crate::resolver::egress::Egress> = std::sync::OnceLock::new();
/// Let the API requests take `egress`'s turn for their host (`www.wikidata.org`), so they go one
/// at a time with the resolver's and at the pace it keeps for the host. Set once, by the daemon;
/// a by-hand run paces only by [`API_INTERVAL`].
pub fn share_egress_pace(egress: crate::resolver::egress::Egress) {
    let _ = EGRESS.set(egress);
}
/// The egress's turn for `url`'s host, waiting for it as long as it takes (a queue too long for
/// the egress's own wait is waited out); none without an egress.
async fn egress_turn(
    egress: Option<&crate::resolver::egress::Egress>,
    url: &str,
) -> Option<tokio::sync::OwnedMutexGuard<tokio::time::Instant>> {
    let egress = egress?;
    let host = url::Url::parse(url).ok()?.host_str()?.to_owned();
    loop {
        match egress.host_turn(&host).await {
            Ok(turn) => return Some(turn),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// Waits until [`API_INTERVAL`] has passed since the last API request of this process.
async fn pace() {
    static LAST: tokio::sync::Mutex<Option<tokio::time::Instant>> =
        tokio::sync::Mutex::const_new(None);
    let mut last = LAST.lock().await;
    if let Some(at) = *last {
        tokio::time::sleep_until(at + API_INTERVAL).await;
    }
    *last = Some(tokio::time::Instant::now());
}

/// One change the recent changes list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The item's Q-id (the page title).
    pub qid: String,
    pub at: chrono::DateTime<chrono::Utc>,
}
/// A page of recent changes and where the next one starts (`rccontinue`), if any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub next: Option<String>,
}
/// The recent changes of items (namespace 0) from `start` to `end`, oldest first: edits, new
/// items and log entries (deletions, restores, merges), 500 a page.
pub async fn recent_changes(
    client: &reqwest::Client,
    base: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    next: Option<&str>,
) -> Result<Changes> {
    let start = start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let end = end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut query = vec![
        ("action", "query"),
        ("list", "recentchanges"),
        ("rcnamespace", "0"),
        ("rctype", "edit|new|log"),
        ("rcprop", "title|timestamp"),
        ("rcdir", "newer"),
        ("rclimit", "500"),
        ("rcstart", start.as_str()),
        ("rcend", end.as_str()),
    ];
    if let Some(next) = next {
        query.push(("rccontinue", next));
    }
    let value = api(client, base, &query).await?;
    // An answer without its list (warnings only, another shape) is no page: read as an empty
    // last one, the window's changes would be skipped.
    if !value["query"]["recentchanges"].is_array() {
        return Err(failure("api", base).into());
    }
    Ok(parse_changes(&value))
}
/// A `list=recentchanges` answer.
pub fn parse_changes(value: &Value) -> Changes {
    let changes = value["query"]["recentchanges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let qid = crate::hubs::qid(c["title"].as_str()?)?;
            let at = chrono::DateTime::parse_from_rfc3339(c["timestamp"].as_str()?)
                .ok()?
                .with_timezone(&chrono::Utc);
            Some(Change { qid, at })
        })
        .collect();
    let next = value["continue"]["rccontinue"]
        .as_str()
        .filter(|s| s.len() <= 256)
        .map(str::to_owned);
    Changes { changes, next }
}

/// What `wbgetentities` said of the items asked for.
#[derive(Debug, Default)]
pub struct Entities {
    /// The items found, by their own Q-id (a redirected one under the item it now is).
    pub found: Vec<(String, Value)>,
    /// The items asked for that are gone (deleted) or now redirect to another.
    pub gone: Vec<String>,
    /// The items asked for the answer does not speak of (neither found, missing nor redirected):
    /// nothing is known of them.
    pub unaccounted: Vec<String>,
}
/// The props a full read of an item asks for (what a card is made of).
pub const FULL: &str = "info|labels|descriptions|aliases|claims|sitelinks";
/// The props that say whether an item is gone, redirected or has a Wikipedia article: a short
/// answer for any item.
pub const SITELINKS: &str = "info|sitelinks";
/// The items `qids` (at most 50) with `props` ([`FULL`], [`SITELINKS`]).
pub async fn entities(
    client: &reqwest::Client,
    base: &str,
    qids: &[String],
    cap: usize,
    props: &str,
) -> Result<Entities> {
    let ids = qids.join("|");
    let value = api_capped(
        client,
        base,
        &[
            ("action", "wbgetentities"),
            ("ids", ids.as_str()),
            ("props", props),
        ],
        cap,
    )
    .await?;
    Ok(parse_entities(value, qids))
}
/// A `wbgetentities` answer for `asked`. An item is gone only when the answer says it is missing
/// or redirected; one it does not speak of is `unaccounted`.
pub fn parse_entities(mut value: Value, asked: &[String]) -> Entities {
    let mut out = Entities::default();
    let asked_set: std::collections::BTreeSet<&str> = asked.iter().map(String::as_str).collect();
    // The items asked for the answer speaks of: found, missing, or redirected to another.
    let mut answered: std::collections::BTreeSet<String> = Default::default();
    let entities = value.get_mut("entities").map(Value::take);
    if let Some(Value::Object(map)) = entities {
        for (key, entity) in map {
            if entity.get("missing").is_some() {
                if asked_set.contains(key.as_str()) {
                    answered.insert(key.clone());
                    out.gone.push(key);
                }
                continue;
            }
            let Some(id) = entity["id"].as_str().and_then(crate::hubs::qid) else {
                continue;
            };
            // An entity without its sitelinks is a degraded answer, not an item that lost its
            // articles: it accounts for nothing (asked again, then the run fails), and its card
            // stays.
            let sitelinks = entity.get("sitelinks");
            if !sitelinks.is_some_and(|l| l.is_object() || l.as_array().is_some_and(Vec::is_empty))
            {
                continue;
            }
            // An item asked for, or the item one asked for now redirects to (listed under its own
            // id with `redirects`, or under the id asked for); nothing else.
            let from = entity["redirects"]["from"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| key.clone());
            if !asked_set.contains(id.as_str()) && !asked_set.contains(from.as_str()) {
                continue;
            }
            if from != id && asked_set.contains(from.as_str()) {
                answered.insert(from.clone());
                out.gone.push(from);
            }
            if asked_set.contains(id.as_str()) {
                answered.insert(id.clone());
            }
            if out.found.iter().all(|(q, _)| *q != id) {
                out.found.push((id, entity));
            }
        }
    }
    out.unaccounted = asked
        .iter()
        .filter(|q| !answered.contains(*q))
        .cloned()
        .collect();
    out.gone.sort();
    out.gone.dedup();
    out
}
/// The English (else `mul`) labels of the items `qids` (at most 50), as tag labels.
pub async fn labels(
    client: &reqwest::Client,
    base: &str,
    qids: &[String],
) -> Result<Vec<(String, Option<String>)>> {
    let ids = qids.join("|");
    let value = api(
        client,
        base,
        &[
            ("action", "wbgetentities"),
            ("ids", ids.as_str()),
            ("props", "labels"),
            ("languages", "en|mul"),
        ],
    )
    .await?;
    Ok(parse_labels(&value, qids))
}
/// A `wbgetentities` answer of labels: each item asked for, with its label as a tag label or
/// none (no English label, too long, or the item is gone).
pub fn parse_labels(value: &Value, asked: &[String]) -> Vec<(String, Option<String>)> {
    // By the id asked for: an entity listed under it, or one that says it was redirected from it.
    let mut by_asked: std::collections::BTreeMap<&str, &Value> = Default::default();
    for (key, entity) in value["entities"].as_object().into_iter().flatten() {
        by_asked.insert(key.as_str(), entity);
        if let Some(from) = entity["redirects"]["from"].as_str() {
            by_asked.insert(from, entity);
        }
    }
    asked
        .iter()
        .map(|qid| {
            let label = by_asked.get(qid.as_str()).and_then(|entity| {
                ["en", "mul"]
                    .iter()
                    .find_map(|l| entity["labels"][*l]["value"].as_str())
                    .and_then(crate::catalogue::label)
            });
            (qid.clone(), label)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A range is never written through a link put in the dump file's place.
    #[tokio::test]
    async fn a_range_is_not_written_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.txt");
        std::fs::write(&target, b"keep me").unwrap();
        let link = dir.path().join("wikidata-20261123-all.json.bz2");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dump = Dump {
            date: "20261123".into(),
            url: "https://dumps.wikimedia.org/x".into(),
            size: 10,
            validator: "\"v\"".into(),
        };
        let error = read_range(&reqwest::Client::new(), &dump, &link, 0, 10, SLOW_WINDOW)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Failure>().map(|f| f.class),
            Some("disk")
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
    }

    #[test]
    fn the_newest_dated_directory_comes_first() {
        let page = r#"<a href="../">../</a><a href="20261116/">20261116/</a>
            <a href="20261123/">x</a><a href="latest-all.json.bz2">l</a><a href="20261399/">bad</a>
            <a href="2026112/">short</a><a href="20261120/">y</a><a href="20261123/">again</a>"#;
        assert_eq!(
            dated_directories(page),
            ["20261123", "20261120", "20261116"]
        );
        assert!(dated_directories("<html></html>").is_empty());
    }
    #[test]
    fn checksums_name_their_file() {
        let sums = "eb604708a07d2d6ea2755ec575a209ba609225d6  wikidata-20261123-all.json.gz\n2652AE0ADA0F8A97A6EA6B7D78DC8EADD5AD41FF  wikidata-20261123-all.json.bz2\n";
        assert_eq!(
            listed_sha1(sums, "wikidata-20261123-all.json.bz2").as_deref(),
            Some("2652ae0ada0f8a97a6ea6b7d78dc8eadd5ad41ff")
        );
        assert_eq!(listed_sha1(sums, "wikidata-20261116-all.json.bz2"), None);
        assert_eq!(
            listed_sha1(
                "xyz  wikidata-20261123-all.json.bz2",
                "wikidata-20261123-all.json.bz2"
            ),
            None
        );
        let dump = Dump {
            date: "20261123".into(),
            url: "https://dumps.wikimedia.org/wikidatawiki/entities/20261123/wikidata-20261123-all.json.bz2".into(),
            size: 1,
            validator: String::new(),
        };
        assert_eq!(
            dump.sums_url().as_deref(),
            Some("https://dumps.wikimedia.org/wikidatawiki/entities/20261123/wikidata-20261123-sha1sums.txt")
        );
        assert_eq!(
            dump.time().unwrap().to_rfc3339(),
            "2026-11-23T00:00:00+00:00"
        );
    }
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
    /// The API's real answer for an item without any sitelink (`fixtures/wikidata`): the item
    /// is found, with no Wikipedia article; an empty list is read the same way.
    #[test]
    fn an_item_without_sitelinks_is_found_without_an_article() {
        let answer: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/wikidata/no-sitelinks.json"
        ))
        .unwrap();
        let asked = ["Q26778405".to_owned()];
        let got = parse_entities(answer.clone(), &asked);
        assert!(got.unaccounted.is_empty() && got.gone.is_empty());
        assert_eq!(got.found.len(), 1);
        assert!(super::super::entity::wikipedia_articles(&got.found[0].1).is_empty());
        let mut listed = answer;
        listed["entities"]["Q26778405"]["sitelinks"] = serde_json::json!([]);
        assert!(parse_entities(listed, &asked).unaccounted.is_empty());
    }
    /// A read keeps going while it brings 8 MB in every ten minutes of waiting for the server;
    /// slower, it ends.
    #[test]
    fn a_read_slower_than_its_floor_ends() {
        let secs = Duration::from_secs;
        let mut speed = Speed::new(SLOW_WINDOW);
        assert!(speed.read(SLOW_BYTES - 1, secs(300)));
        // 8 MB in: the window starts again.
        assert!(speed.read(1, secs(290)));
        assert!(speed.read(1 << 20, SLOW_WINDOW));
        assert!(!speed.read(1, secs(1)));
    }
    /// A host that asked to be left alone longer than five minutes is not asked again before.
    #[test]
    fn a_long_wait_asked_holds_every_request_to_the_host() {
        let url = "https://example.org:4443/w/api.php";
        assert!(!too_early(url));
        wait_asked(url, Duration::from_secs(3600));
        assert!(too_early(url) && too_early("https://example.org:4443/other"));
        assert!(!too_early("https://example.org/w/api.php"));
        forget_wait_asked(url);
        assert!(!too_early(url));
        // Years are held a day at most, and never overflow.
        wait_asked(url, Duration::from_secs(u64::MAX));
        assert!(too_early(url));
        let until = NOT_BEFORE.lock().unwrap()["example.org:4443"];
        assert!(until <= std::time::Instant::now() + LONGEST_HOLD);
        forget_wait_asked(url);
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
    fn recent_changes_and_entities_are_read_from_their_answers() {
        let page = serde_json::json!({
            "continue": {"rccontinue": "20261125101500|123", "continue": "-||"},
            "query": {"recentchanges": [
                {"type": "edit", "ns": 0, "title": "Q42", "timestamp": "2026-11-25T10:14:59Z"},
                {"type": "log", "ns": 0, "title": "Q0", "timestamp": "2026-11-25T10:15:00Z"},
                {"type": "new", "ns": 0, "title": "Property:P31", "timestamp": "2026-11-25T10:15:00Z"},
                {"type": "new", "ns": 0, "title": "Q7", "timestamp": "not a time"}
            ]}
        });
        let changes = parse_changes(&page);
        assert_eq!(changes.changes.len(), 1);
        assert_eq!(changes.changes[0].qid, "Q42");
        assert_eq!(changes.next.as_deref(), Some("20261125101500|123"));
        let answer = serde_json::json!({"entities": {
            "Q1": {"type": "item", "id": "Q1", "sitelinks": {}},
            "Q2": {"id": "Q2", "missing": ""},
            "Q30": {"type": "item", "id": "Q30", "sitelinks": {}, "redirects": {"from": "Q3", "to": "Q30"}},
            "Q99": {"type": "item", "id": "Q99", "sitelinks": {}}
        }});
        let asked = ["Q1".to_owned(), "Q2".to_owned(), "Q3".to_owned()];
        let got = parse_entities(answer, &asked);
        let found: Vec<&str> = got.found.iter().map(|(q, _)| q.as_str()).collect();
        // Q99, which nobody asked for, is not taken.
        assert_eq!(found, ["Q1", "Q30"]);
        assert_eq!(got.gone, ["Q2", "Q3"]);
        assert!(got.unaccounted.is_empty());
        // Items an answer leaves out are unaccounted for, never gone.
        let partial =
            serde_json::json!({"entities": {"Q1": {"type": "item", "id": "Q1", "sitelinks": {}}}});
        let got = parse_entities(partial, &asked);
        assert_eq!(
            (got.gone.len(), got.unaccounted),
            (0, vec!["Q2".to_owned(), "Q3".to_owned()])
        );
        let got = parse_entities(serde_json::json!({"x": 1}), &asked);
        assert_eq!(got.unaccounted, asked);
        // An entity without its sitelinks is no answer for it either.
        let bare = serde_json::json!({"entities": {
            "Q1": {"type": "item", "id": "Q1"},
            "Q2": {"id": "Q2", "missing": ""},
            "Q3": {"type": "item", "id": "Q3", "sitelinks": {}}
        }});
        let got = parse_entities(bare, &asked);
        assert_eq!(got.unaccounted, ["Q1"]);
        assert_eq!(got.gone, ["Q2"]);
        let labels = serde_json::json!({"entities": {
            "Q11399": {"labels": {"en": {"value": "Rock Music"}}},
            "Q9": {"labels": {"mul": {"value": "a label far too long for a tag label"}}}
        }});
        assert_eq!(
            parse_labels(&labels, &["Q11399".into(), "Q9".into(), "Q404".into()]),
            [
                ("Q11399".to_owned(), Some("rock-music".to_owned())),
                ("Q9".to_owned(), None),
                ("Q404".to_owned(), None)
            ]
        );
    }

    /// A JSON value of any shape, a few levels deep.
    fn json() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            "[A-Za-z0-9|_ :-]{0,12}".prop_map(Value::String),
            prop::sample::select(vec![
                "Q42",
                "Q0",
                "P31",
                "2026-11-25T10:00:00Z",
                "missing",
                "entities",
                "redirects"
            ])
            .prop_map(|s| Value::String(s.into())),
        ];
        leaf.prop_recursive(4, 64, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map(
                    prop::sample::select(vec![
                        "entities",
                        "query",
                        "recentchanges",
                        "continue",
                        "rccontinue",
                        "title",
                        "timestamp",
                        "id",
                        "labels",
                        "en",
                        "mul",
                        "value",
                        "redirects",
                        "from",
                        "missing",
                        "Q42",
                        "Q1"
                    ])
                    .prop_map(str::to_owned),
                    inner,
                    0..6
                )
                .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 2000, ..Default::default() })]
        /// Fuzz: API answers of any shape and pages of any text are read without a panic, and
        /// what they give is well-formed (Q-ids, an answer only for the items asked for).
        #[test]
        fn answers_of_any_shape_are_read_without_a_panic(value in json(), page in "\\PC{0,200}", sums in "[0-9a-f ]{0,80}\n?[a-z0-9.-]{0,40}") {
            let changes = parse_changes(&value);
            for c in &changes.changes {
                proptest::prop_assert!(crate::hubs::qid(&c.qid).is_some());
            }
            let asked = vec!["Q42".to_owned(), "Q1".to_owned()];
            let got = parse_entities(value.clone(), &asked);
            proptest::prop_assert!(got.found.len() <= 2 + asked.len());
            proptest::prop_assert!(got.gone.iter().all(|q| asked.contains(q)));
            proptest::prop_assert!(got.unaccounted.iter().all(|q| asked.contains(q) && !got.gone.contains(q)));
            let labels = parse_labels(&value, &asked);
            proptest::prop_assert_eq!(labels.len(), 2);
            for (_, l) in labels.iter().filter_map(|(q, l)| l.as_ref().map(|l| (q, l))) {
                let made = crate::catalogue::label(l);
                proptest::prop_assert_eq!(made.as_deref(), Some(l.as_str()));
            }
            for d in dated_directories(&page) {
                proptest::prop_assert!(d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()));
            }
            if let Some(sum) = listed_sha1(&sums, "x.json.bz2") {
                proptest::prop_assert_eq!(sum.len(), 40);
            }
        }
    }
}
