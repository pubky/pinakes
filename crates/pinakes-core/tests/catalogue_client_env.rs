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
    // No address for the name: refused at once, or a black-holed lookup that times out.
    assert!(
        matches!(class(&far), "connect" | "timeout"),
        "{}",
        class(&far)
    );
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
