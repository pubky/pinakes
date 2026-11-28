//! The catalogues' client ignores proxies named in the environment. Its own test binary: the
//! test sets proxy variables, which are the whole process's.
use axum::{routing::get, Router};
use pinakes_core::catalogue::{failure::Failure, fetch};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

/// Proxies named in the environment are never asked: a proxy listener sees no connection, for a
/// loopback server that answers nor for a closed https port (which only a proxy could answer).
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
    // A closed https port on the loopback address: asked directly, the connection is refused
    // (no name to look up); only a proxy would take the request.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let far = format!("https://{}/x", closed.local_addr().unwrap());
    drop(closed);
    let failed = client.small(&far, 100).await.unwrap_err();
    assert_eq!(class(&failed), "connect");
    let api = fetch::api_client("ops@example.org", &[])
        .unwrap()
        .first_wait(Duration::from_millis(1));
    let failed = api.small(&far, 100).await.unwrap_err();
    assert_eq!(class(&failed), "connect");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}
