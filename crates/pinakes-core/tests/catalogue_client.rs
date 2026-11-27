//! The one HTTP client of the catalogue importers (`catalogue::fetch::client`) and the downloader
//! they share (`catalogue::bulk::Download`): redirects only to the hosts a catalogue names,
//! proxies named in the environment ignored, `https` only (plain `http` on a loopback address),
//! `Retry-After` waited for, a slow server cut off, and one `--from` rule for every importer.
//! Its own test binary: one test sets proxy variables for the whole process.
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use pinakes_core::catalogue::{
    bulk::Download,
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

/// Proxies named in the environment are never asked: a proxy listener sees no connection, for an
/// `https` host (which only a proxy could reach here) nor for a loopback one.
#[tokio::test]
async fn proxies_named_in_the_environment_are_ignored() {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    proxy.set_nonblocking(true).unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    std::thread::spawn(move || loop {
        match proxy.accept() {
            Ok(_) => {
                counted.fetch_add(1, Ordering::SeqCst);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    });
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        std::env::set_var(name, &proxy_url);
    }
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");
    let origin = serve(Router::new().route("/x", get(|| async { "direct" }))).await;
    let client = fetch::client("ops@example.org", &[])
        .unwrap()
        .first_wait(Duration::from_millis(1));
    let direct = client.small(&format!("{origin}/x"), 100).await.unwrap();
    assert_eq!(direct.as_deref(), Some(&b"direct"[..]));
    // A name that resolves nowhere: only a proxy would take the request.
    let far = client
        .small("https://pinakes-proxy-test.invalid/x", 100)
        .await
        .unwrap_err();
    assert_eq!(class(&far), "connect");
    let api = fetch::api_client("ops@example.org", &[])
        .unwrap()
        .first_wait(Duration::from_millis(1));
    assert!(api
        .small("https://pinakes-proxy-test.invalid/w/api.php", 100)
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(connections.load(Ordering::SeqCst), 0);
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
