//! What the catalogues of bulk files share: where a file comes from ([`Input`]: the official URL,
//! another URL, or a local file for `pinakesd catalogue import --from` and the tests), the HTTP
//! client that asks for it ([`client`]), the version a file's answer names without reading it
//! ([`Input::version`]), and the bridge from a blocking reader on its own thread to the run's
//! writes ([`batches`]).
//!
//! The URLs are the operator's or the build's, never a reference's: no request here is steered
//! by what people share, so the client is not the egress. It follows a redirect only to an
//! `https` URL on a host its catalogue names ([`client`]), names the operator's contact in its
//! User-Agent, and every body is read within a size cap and a deadline (`bulk::Download`).
use super::{bulk, failure::Failure, Card};
use anyhow::Result;
use std::{io::Read, path::PathBuf, time::Duration};

/// Where a catalogue file is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Url(url::Url),
    /// A local file (`--from FILE`).
    File(PathBuf),
}
impl Input {
    /// A URL (`http`/`https`) or a local path.
    pub fn parse(from: &str) -> Result<Self> {
        if from.starts_with("http://") || from.starts_with("https://") {
            // Never echoed: a URL may carry a credential.
            let url = url::Url::parse(from).map_err(|_| anyhow::anyhow!("--from: not a URL"))?;
            anyhow::ensure!(
                url.username().is_empty() && url.password().is_none(),
                "--from: a URL with a user name or password is refused"
            );
            Ok(Input::Url(url))
        } else {
            Ok(Input::File(PathBuf::from(from)))
        }
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
        client: &reqwest::Client,
    ) -> Result<(String, Option<chrono::DateTime<chrono::Utc>>)> {
        match self {
            Input::Url(url) => {
                let sent = tokio::time::timeout(WAIT, client.head(url.clone()).send()).await;
                let answer = match sent {
                    Ok(Ok(answer)) if answer.status().is_success() => answer,
                    Ok(Ok(answer)) if answer.status().as_u16() == 404 => {
                        anyhow::bail!(Failure::new("not_found").at(self.at()))
                    }
                    _ => anyhow::bail!(Failure::new("download").at(self.at())),
                };
                let header = |name| {
                    answer
                        .headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned)
                };
                let modified = header(reqwest::header::LAST_MODIFIED);
                let at = modified
                    .as_deref()
                    .and_then(|m| httpdate::parse_http_date(m).ok())
                    .map(chrono::DateTime::<chrono::Utc>::from);
                let version = header(reqwest::header::ETAG)
                    .filter(|e| !e.starts_with("W/"))
                    .or(modified)
                    .ok_or_else(|| Failure::new("version").at(self.at()))?;
                let mut version = version;
                crate::model::truncate(&mut version, 200);
                Ok((version, at))
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
        client: &reqwest::Client,
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
    pub async fn read_small(&self, client: &reqwest::Client, max_bytes: u64) -> Result<Vec<u8>> {
        match self {
            Input::Url(url) => {
                let sent = tokio::time::timeout(WAIT, client.get(url.clone()).send()).await;
                let mut answer = match sent {
                    Ok(Ok(answer)) if answer.status().as_u16() == 200 => answer,
                    Ok(Ok(answer)) if answer.status().as_u16() == 404 => {
                        anyhow::bail!(Failure::new("not_found").at(self.at()))
                    }
                    _ => anyhow::bail!(Failure::new("download").at(self.at())),
                };
                let mut body = Vec::new();
                loop {
                    match tokio::time::timeout(WAIT, answer.chunk()).await {
                        Ok(Ok(Some(chunk))) => {
                            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                                anyhow::bail!(Failure::new("size").at(self.at()));
                            }
                            body.extend_from_slice(&chunk);
                        }
                        Ok(Ok(None)) => return Ok(body),
                        _ => anyhow::bail!(Failure::new("download").at(self.at())),
                    }
                }
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
/// The longest wait for a small file's answer or one of its chunks.
const WAIT: Duration = Duration::from_secs(120);
/// The first wait before a broken download is asked for again (it doubles each time).
const RETRY_WAIT: Duration = Duration::from_secs(5);

/// The client a catalogue asks for its files with: the operator's contact in its User-Agent,
/// connection and answer timeouts, and redirects followed (at most 3) only to `https` URLs on
/// `hosts` (a mirror's own CDN host): a redirect anywhere else is an error.
pub fn client(contact: &str, hosts: &'static [&'static str]) -> Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        let allowed = attempt.previous().len() < 3
            && attempt.url().scheme() == "https"
            && attempt.url().port().is_none()
            && attempt
                .url()
                .host_str()
                .is_some_and(|host| hosts.contains(&host));
        if allowed {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    // No compressed answers: a file's bytes are what its publisher serves, its length known
    // (`bulk::Download` needs it), and nothing is unpacked here that the caps don't see.
    Ok(reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .redirect(policy)
        .user_agent(crate::resolver::egress::user_agent(contact))
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

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
