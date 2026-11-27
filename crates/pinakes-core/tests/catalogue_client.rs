//! The one HTTP client of the catalogue importers (`catalogue::fetch::client`) and the downloader
//! they share (`catalogue::bulk::Download`): redirects only to the hosts a catalogue names,
//! proxies named in the environment ignored, `https` only (plain `http` on a loopback address),
//! `Retry-After` waited for, a slow server cut off, and one `--from` rule for every importer.
//! (Proxy variables, which are the whole process's, are tested in `catalogue_client_env.rs`.)
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use pinakes_core::catalogue::{
    bulk::{self, Download},
    failure::Failure,
    fetch::{self, Input},
    musicbrainz, wikidata,
};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}")
}
fn class(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<Failure>()
        .or_else(|| error.chain().find_map(|c| c.downcast_ref::<Failure>()))
        .map_or("none", |f| f.class)
}
fn io_class(error: &std::io::Error) -> &'static str {
    error
        .get_ref()
        .and_then(|e| e.downcast_ref::<Failure>())
        .map_or("none", |f| f.class)
}

/// A redirect is followed only to a host the catalogue names: one to another host (here the same
/// server by another name, which would answer) is not, neither for a small answer nor for a
/// download.
#[tokio::test]
async fn a_redirect_to_a_host_off_the_list_is_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let counted = hits.clone();
    let app =
        Router::new()
            .route(
                "/away",
                get(move || async move {
                    Redirect::temporary(&format!("http://localhost:{port}/file"))
                }),
            )
            .route(
                "/near",
                get(move || async move {
                    Redirect::temporary(&format!("http://127.0.0.1:{port}/file"))
                }),
            )
            .route(
                "/file",
                get(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        "the file"
                    }
                }),
            );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");
    let client = fetch::client("ops@example.org", &["127.0.0.1"]).unwrap();
    // A listed host: followed.
    let near = client.small(&format!("{base}/near"), 1000).await.unwrap();
    assert_eq!(near.as_deref(), Some(&b"the file"[..]));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // Another host: not followed, and the redirect is no file.
    let away = client
        .small(&format!("{base}/away"), 1000)
        .await
        .unwrap_err();
    assert_eq!(class(&away), "download");
    let url = url::Url::parse(&format!("{base}/away")).unwrap();
    let handle = tokio::runtime::Handle::current();
    let (c, h) = (client.clone(), handle.clone());
    let opened = tokio::task::spawn_blocking(move || {
        Download::open(c, h, url, 1 << 20, Duration::from_millis(1)).map(|_| ())
    })
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(class(&opened), "download");
    // With no hosts, no redirect at all.
    let none = fetch::client("ops@example.org", &[]).unwrap();
    let refused = none.small(&format!("{base}/near"), 1000).await.unwrap_err();
    assert_eq!(class(&refused), "download");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A rate limit or a server error is asked again after the time its `Retry-After` names; one
/// asking longer than five minutes fails at once (`rate_limited`), and the host is left alone
/// until then. A download waits the same way.
#[tokio::test]
async fn a_retry_after_is_honoured() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let app = Router::new()
        .route(
            "/soon",
            get(move || {
                let counted = counted.clone();
                async move {
                    if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "1")],
                            "wait",
                        )
                            .into_response()
                    } else {
                        "now".into_response()
                    }
                }
            }),
        )
        .route(
            "/file",
            get({
                let seen = Arc::new(AtomicUsize::new(0));
                move || {
                    let seen = seen.clone();
                    async move {
                        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("retry-after", "1")],
                                "busy",
                            )
                                .into_response()
                        } else {
                            ([("etag", "\"v1\"")], vec![b'x'; 1000]).into_response()
                        }
                    }
                }
            }),
        );
    let base = serve(app).await;
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let started = Instant::now();
    let body = client.small(&format!("{base}/soon"), 100).await.unwrap();
    assert_eq!(body.as_deref(), Some(&b"now"[..]));
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // A download is asked again as its answer says.
    let url = url::Url::parse(&format!("{base}/file")).unwrap();
    let (c, h) = (client.clone(), tokio::runtime::Handle::current());
    let started = Instant::now();
    let read = tokio::task::spawn_blocking(move || {
        let mut body = Download::open(c, h, url, 1 << 20, Duration::from_millis(1))?;
        let mut out = vec![];
        body.read_to_end(&mut out)?;
        anyhow::Ok(out.len())
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(read, 1000);
    assert!(started.elapsed() >= Duration::from_secs(1));

    // An hour asked: no wait, the run fails, and the host is not asked again meanwhile.
    let long = Arc::new(AtomicUsize::new(0));
    let counted = long.clone();
    let base = serve(Router::new().route(
        "/later",
        get(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("retry-after", "3600")],
                    "later",
                )
            }
        }),
    ))
    .await;
    let started = Instant::now();
    let failed = client
        .small(&format!("{base}/later"), 100)
        .await
        .unwrap_err();
    assert_eq!(class(&failed), "rate_limited");
    assert!(started.elapsed() < Duration::from_secs(5));
    let again = client.head(&format!("{base}/other")).await.unwrap_err();
    assert_eq!(class(&again), "rate_limited");
    assert_eq!(long.load(Ordering::SeqCst), 1);
    // The hold is the process's: no test leaves one behind for a port handed out again.
    fetch::forget_hold(&format!("{base}/later"));
    assert!(client.head(&format!("{base}/other")).await.is_ok());
}

/// A server slower than the floor (8 MB in the slow window of waiting) fails the download; the
/// reader's own pauses don't count.
#[tokio::test]
async fn a_slow_server_fails_the_download() {
    let app = Router::new().route(
        "/slow",
        get(|| async {
            let chunks = futures_util::stream::unfold(0u32, |n| async move {
                if n >= 50 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some((Ok::<_, std::io::Error>(vec![b'x'; 10]), n + 1))
            });
            Response::builder()
                .header("content-length", "500")
                .header("etag", "\"v\"")
                .body(axum::body::Body::from_stream(chunks))
                .unwrap()
        }),
    );
    let base = serve(app).await;
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let url = url::Url::parse(&format!("{base}/slow")).unwrap();
    let handle = tokio::runtime::Handle::current();
    let failed = tokio::task::spawn_blocking(move || {
        let mut body = Download::new(client, handle, url, 1 << 20).slow(Duration::from_millis(700));
        let mut out = vec![];
        body.read_to_end(&mut out).map(|_| out.len())
    })
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(io_class(&failed), "download");
}

/// Every importer's `--from` takes the same URLs: `https://` (`http://` on a loopback address
/// only), without a user name, a password or a query, none of which is repeated in the error; and
/// the client asks no URL outside that rule (`url`), whatever its caller.
#[tokio::test]
async fn one_from_rule_for_every_importer() {
    let refused = [
        "http://dumps.example.org/x.json.bz2",
        "https://dumps.example.org/x.json.bz2?token=s3cr3t",
        "https://user:s3cr3t@dumps.example.org/x.json.bz2",
        "https://s3cr3t@dumps.example.org/x.json.bz2",
        "ftp://dumps.example.org/s3cr3t",
    ];
    for from in refused {
        let errors = [
            fetch::from_url(from).map(|_| ()).unwrap_err(),
            Input::parse(from).map(|_| ()).unwrap_err(),
            musicbrainz::Source::parse(from).map(|_| ()).unwrap_err(),
            wikidata::From::parse(from).map(|_| ()).unwrap_err(),
        ];
        for error in errors {
            let text = format!("{error:#}");
            assert!(!text.contains("s3cr3t"), "{from}: {text}");
        }
    }
    for from in [
        "https://dumps.example.org/x.json.bz2",
        "http://127.0.0.1:8080/x.json.bz2",
        "http://localhost:8080/x.json.bz2",
        "http://[::1]:8080/x.json.bz2",
    ] {
        assert!(fetch::from_url(from).unwrap().is_some(), "{from}");
        assert!(
            matches!(Input::parse(from).unwrap(), Input::Url(_)),
            "{from}"
        );
        assert!(wikidata::From::parse(from).is_ok(), "{from}");
    }
    // A path is no URL.
    assert_eq!(fetch::from_url("/data/x.json.bz2").unwrap(), None);
    let client = fetch::client("ops@example.org", &[]).unwrap();
    for url in [
        "http://example.org/x",
        "https://user@example.org/x",
        "file:///etc/passwd",
    ] {
        let failed = client.small(url, 100).await.unwrap_err();
        assert_eq!(class(&failed), "url", "{url}");
    }
}

/// The client says who asks (the operator's contact) and asks for no compressed answer; the API
/// client may take one.
#[tokio::test]
async fn the_client_names_its_contact_and_takes_bytes_as_served() {
    let app = Router::new().route(
        "/who",
        get(|headers: HeaderMap| async move {
            let h = |n: &str| {
                headers
                    .get(n)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_owned()
            };
            format!("{}|{}", h("user-agent"), h("accept-encoding"))
        }),
    );
    let base = serve(app).await;
    let plain = fetch::client("ops@example.org", &[]).unwrap();
    let said = plain
        .small(&format!("{base}/who"), 1000)
        .await
        .unwrap()
        .unwrap();
    let said = String::from_utf8(said).unwrap();
    assert!(said.contains("ops@example.org"), "{said}");
    assert!(said.ends_with("|none"), "{said}");
    let api = fetch::api_client("ops@example.org", &[]).unwrap();
    let said = api
        .small(&format!("{base}/who"), 1000)
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(said).unwrap().contains("gzip"));
}

/// A server that asks to wait half a minute, counting what it is asked.
async fn busy() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let base = serve(Router::new().route(
        "/busy",
        get(move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("retry-after", "30")],
                    "busy",
                )
            }
        }),
    ))
    .await;
    (format!("{base}/busy"), hits)
}

/// A known file read from its end asks nothing (a range already read whole).
#[tokio::test]
async fn a_range_read_whole_asks_nothing() {
    let (url, hits) = busy().await;
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let handle = tokio::runtime::Handle::current();
    let url = url::Url::parse(&url).unwrap();
    let read = tokio::task::spawn_blocking(move || {
        let mut out = vec![];
        let end = Download::new(client.clone(), handle.clone(), url.clone(), 100)
            .known(10, "\"v\"")
            .from(10)
            .read_to_end(&mut out)?;
        let empty = Download::new(client, handle, url, 100)
            .known(10, "\"v\"")
            .from(4)
            .until(4)
            .read_to_end(&mut out)?;
        std::io::Result::Ok(end + empty)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(read, 0);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

/// A download that waits is stopped within about a second once `flag` is set (`stopped`).
async fn stopped_soon(
    flag: Arc<std::sync::atomic::AtomicBool>,
    read: tokio::task::JoinHandle<std::io::Result<usize>>,
) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    flag.store(true, Ordering::SeqCst);
    let failed = read.await.unwrap().unwrap_err();
    assert_eq!(io_class(&failed), "stopped");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// A stop is heard within about a second in a `Retry-After` of half a minute.
#[tokio::test]
async fn a_stop_is_heard_in_a_retry_after() {
    let handle = tokio::runtime::Handle::current();
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let (busy_url, _) = busy().await;
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c, h, u, f) = (
        client.clone(),
        handle.clone(),
        url::Url::parse(&busy_url).unwrap(),
        flag.clone(),
    );
    let read = tokio::task::spawn_blocking(move || {
        Download::new(c, h, u, 100).stop(f).read_to_end(&mut vec![])
    });
    stopped_soon(flag, read).await;
}

/// A stop is heard within about a second in the pause before a resume: a body that breaks off,
/// asked for again after half a minute.
#[tokio::test]
async fn a_stop_is_heard_in_the_pause_before_a_resume() {
    let handle = tokio::runtime::Handle::current();
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let broken = format!("http://{}/file", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nETag: \"v\"\r\n\r\n0123456789",
                )
                .await;
            let _ = socket.shutdown().await;
        }
    });
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c, h, u, f) = (
        client.clone(),
        handle.clone(),
        url::Url::parse(&broken).unwrap(),
        flag.clone(),
    );
    let read = tokio::task::spawn_blocking(move || {
        Download::new(c, h, u, 1000)
            .retry_wait(Duration::from_secs(30))
            .stop(f)
            .read_to_end(&mut vec![])
    });
    stopped_soon(flag, read).await;
}

/// A reader that read ahead, dropped while its download waits (a `Retry-After` of half a
/// minute): the download is stopped and its thread ends within about a second.
#[tokio::test]
async fn a_dropped_read_ahead_stops_its_thread() {
    let handle = tokio::runtime::Handle::current();
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let (busy_url, hits) = busy().await;
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c, h, u, f) = (
        client.clone(),
        handle.clone(),
        url::Url::parse(&busy_url).unwrap(),
        flag.clone(),
    );
    let ahead =
        tokio::task::spawn_blocking(move || Download::new(c, h, u, 100).stop(f).read_ahead(4))
            .await
            .unwrap();
    let finished = ahead.finished();
    // Dropped once the download waits in its `Retry-After`, not before it asked.
    let started = Instant::now();
    while hits.load(Ordering::SeqCst) == 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "never asked");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!finished.load(Ordering::SeqCst));
    drop(ahead);
    assert!(flag.load(Ordering::SeqCst));
    // The reading thread itself ends, within about a second.
    let started = Instant::now();
    while !finished.load(Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the read-ahead thread still runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A file catalogue's reader whose run is gone (its receiver dropped) while its download waits
/// (a `Retry-After` of half a minute) ends within about a second.
#[tokio::test]
async fn a_file_reader_whose_run_is_gone_ends() {
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let (busy_url, _) = busy().await;
    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    let (c, u) = (client.clone(), busy_url.clone());
    let parts = fetch::batches(move |_sink| {
        let input = Input::parse(&u).unwrap();
        let handle = tokio::runtime::Handle::current();
        let opened = input.open(&c, &handle, 100).map(|_| ());
        let _ = ended_tx.send(Instant::now());
        opened
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Still waiting when the run goes: it is the drop that ends it.
    assert!(
        ended_rx.try_recv().is_err(),
        "the reader ended before the drop"
    );
    let dropped = Instant::now();
    drop(parts);
    let ended = tokio::task::spawn_blocking(move || ended_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the reader ended");
    assert!(ended.duration_since(dropped) < Duration::from_secs(3));
}

/// A download read ahead gives its bytes in order and whole.
#[tokio::test]
async fn a_download_read_ahead_gives_its_bytes_in_order() {
    let body: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let served = body.clone();
    let base = serve(Router::new().route(
        "/file",
        get(move || {
            let served = served.clone();
            async move { ([("etag", "\"v\"")], served) }
        }),
    ))
    .await;
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let handle = tokio::runtime::Handle::current();
    let url = url::Url::parse(&format!("{base}/file")).unwrap();
    let read = tokio::task::spawn_blocking(move || {
        let mut out = vec![];
        Download::new(client, handle, url, 2_000_000)
            .read_ahead(4)
            .read_to_end(&mut out)
            .map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(read, body);
}

/// A reader that panics leaves no stop on its (pooled) thread: a later download there, made
/// without a stop of its own, reads.
#[tokio::test]
async fn a_panicking_reader_leaves_no_stop_on_its_thread() {
    let base = serve(Router::new().route("/x", get(|| async { "fine" }))).await;
    let client = fetch::client("ops@example.org", &[]).unwrap();
    let handle = tokio::runtime::Handle::current();
    let url = url::Url::parse(&format!("{base}/x")).unwrap();
    // On a pooled blocking thread, as a file catalogue's reader runs.
    let read = tokio::task::spawn_blocking(move || {
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let panicked = std::panic::catch_unwind(|| {
            bulk::stopping_with::<()>(stopped, || panic!("a reader's bug"))
        });
        assert!(panicked.is_err());
        assert!(bulk::thread_stop().is_none());
        let mut out = String::new();
        Download::new(client, handle, url, 100)
            .read_to_string(&mut out)
            .map(|_| out)
    })
    .await
    .unwrap();
    assert_eq!(read.unwrap(), "fine");
}

/// A read-ahead thread that dies is a failure (`read`), never the stream's end; one whose reader
/// ends gives every byte and then the end.
#[test]
fn a_read_ahead_thread_that_dies_is_a_failure() {
    struct Dies(u8);
    impl Read for Dies {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                panic!("a reader's bug");
            }
            self.0 -= 1;
            buf[..3].copy_from_slice(b"abc");
            Ok(3)
        }
    }
    let flag = || Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut out = vec![];
    let failed = bulk::read_ahead(Dies(2), 4, flag())
        .read_to_end(&mut out)
        .unwrap_err();
    assert_eq!(io_class(&failed), "read");
    let mut out = vec![];
    bulk::read_ahead(&b"abcdef"[..], 4, flag())
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, b"abcdef");
}
