use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::{
    config::{EgressConfig, QueueConfig},
    model::{ResourceMeta, Status},
    queue,
    resolver::{
        egress::{Dns, Egress, Fetched, Transport},
        interstitial::{self, Page},
        web, Resolver,
    },
    store, telemetry,
};
use sqlx::PgPool;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use url::Url;

fn page(uri: &str, body: &str, status: u16) -> Fetched {
    Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: uri.parse().unwrap(),
        content_type: "text/html".into(),
        status,
        retry_after_secs: 60,
        location: None,
        body: body.as_bytes().to_vec(),
    }
}
#[test]
fn captured_walls_stubs_and_content_are_distinguished() {
    let requested = Url::parse("https://www.federalregister.gov/documents/example").unwrap();
    let wall = include_str!("fixtures/interstitials/federal-register-wall.html");
    for status in [200, 403, 429, 503] {
        let f = page("https://unblock.federalregister.gov/", wall, status);
        assert_eq!(interstitial::inspect(&requested, &f), Page::Blocked);
        let d = web::extract(requested.as_str(), &f, 86400).doc;
        assert_eq!(d.status, Status::Blocked);
        assert!(
            d.name.is_none()
                && d.image.is_none()
                && d.description.is_none()
                && d.group_keys.is_empty()
        );
    }
    let uri = "https://pubky.org/explore/pubky-protocol/pkarr/architecture/";
    let f = page(
        uri,
        include_str!("fixtures/interstitials/pubky-redirect.html"),
        200,
    );
    assert_eq!(
        interstitial::inspect(&uri.parse().unwrap(), &f),
        Page::Redirect("/pkarr/".into())
    );
    for body in [
        "<script>window.location.replace('/next')</script>",
        "<title>Redirecting to: /next</title>",
        "<body>Redirecting to <a href='/next'>next</a></body>",
    ] {
        assert_eq!(
            interstitial::inspect(&f.url, &page(uri, body, 200)),
            Page::Redirect("/next".into())
        );
    }
    for marker in [
        "Just a moment",
        "Attention Required",
        "Access Denied",
        "Verify you are human",
        "Enable JavaScript",
        "Unusual traffic",
    ] {
        assert_eq!(
            interstitial::inspect(&f.url, &page(uri, &format!("<body>{marker}</body>"), 200)),
            Page::Blocked
        );
    }
    let ordinary=format!("<title>A real article</title><body>{}<script>const message='Enable JavaScript';</script></body>","Useful text. ".repeat(300));
    assert_eq!(
        interstitial::inspect(&f.url, &page(uri, &ordinary, 200)),
        Page::Content
    );
}

#[tokio::test]
async fn real_http_error_bodies_reach_interstitial_detection_with_a_cap() {
    use axum::{http::StatusCode, routing::get, Router};
    use pinakes_core::resolver::egress::read_response;
    let app = Router::new()
        .route(
            "/forbidden",
            get(|| async {
                (
                    StatusCode::FORBIDDEN,
                    [("content-type", "text/html")],
                    "<title>Request Access</title>",
                )
            }),
        )
        .route(
            "/limited",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("content-type", "text/plain")],
                    "Verify you are human",
                )
            }),
        )
        .route(
            "/unavailable",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("content-type", "text/html")],
                    format!("<title>Just a moment</title>{}", "x".repeat(100_000)),
                )
            }),
        )
        .route(
            "/ordinary",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("content-type", "text/html")],
                    "<title>Maintenance</title>",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    for (path, status, expected) in [
        ("forbidden", 403, Page::Blocked),
        ("limited", 429, Page::Blocked),
        ("unavailable", 503, Page::Blocked),
        ("ordinary", 503, Page::Content),
    ] {
        let uri: Url = format!("http://{address}/{path}").parse().unwrap();
        let f = read_response(uri.clone(), client.get(uri.clone()).send().await.unwrap())
            .await
            .unwrap();
        assert_eq!(f.status, status);
        assert_eq!(interstitial::inspect(&uri, &f), expected);
        if path == "unavailable" {
            assert_eq!(f.body.len(), 64 * 1024);
            assert!(f.truncated);
        }
    }
    server.abort();
}
struct PublicDns;
#[async_trait]
impl Dns for PublicDns {
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
struct Pages {
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl Transport for Pages {
    async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
        self.calls.lock().unwrap().push(u.to_string());
        let mut f = page(u.as_str(), "<title>Destination</title>", 200);
        match u.path() {
            "/start" => {
                f.status = 301;
                f.location = Some("/stub".into());
            }
            "/stub" => f.body = b"<meta http-equiv='refresh' content='0;url=/js'>".to_vec(),
            "/js" => f.body = b"<script>location.replace('/end')</script>".to_vec(),
            "/loop" => f.body = b"<meta http-equiv='refresh' content='0;url=/loop'>".to_vec(),
            "/private" => {
                f.body = b"<script>location.replace('http://private.example/')</script>".to_vec()
            }
            "/permanent" => {
                f.body = b"<title>Redirecting to: /ignored-html-target</title>".to_vec();
                f.status = 308;
                f.location = Some("/end".into());
            }
            "/temporary" => {
                f.status = 302;
                f.location = Some("/end".into());
            }
            "/wall" => {
                f.status = 429;
                f.body = b"<title>Just a moment</title>".to_vec();
            }
            _ => {}
        }
        Ok(f)
    }
}
fn egress(p: Arc<Pages>) -> Egress {
    Egress::with_backends(
        EgressConfig {
            host_requests_per_second: 1000.0,
            ..Default::default()
        },
        Arc::new(PublicDns),
        p,
    )
}
#[tokio::test]
async fn html_hops_share_http_redirect_budget_and_recheck_egress() {
    let p = Arc::new(Pages {
        calls: Mutex::default(),
    });
    let e = egress(p.clone());
    let f = e.fetch("https://public.example/start").await.unwrap();
    assert_eq!(f.url.path(), "/end");
    assert!(!f.permanent_redirect);
    assert_eq!(p.calls.lock().unwrap().len(), 4);
    assert!(e
        .fetch("https://public.example/loop")
        .await
        .unwrap_err()
        .to_string()
        .contains("3 redirects"));
    assert!(e
        .fetch("https://public.example/private")
        .await
        .unwrap_err()
        .to_string()
        .contains("non-public"));
    for (path, permanent) in [("permanent", true), ("temporary", false)] {
        let uri = format!("https://public.example/{path}");
        let f = e.fetch(&uri).await.unwrap();
        assert_eq!(f.permanent_redirect, permanent);
        assert_eq!(
            !web::extract(&uri, &f, 86400).doc.group_keys.is_empty(),
            permanent
        );
    }
    let metrics = telemetry::Metrics::new().unwrap();
    let (result, obs) = telemetry::observe(
        &metrics,
        &pinakes_core::adapter::manifest("web"),
        e.fetch("https://public.example/wall"),
    )
    .await;
    assert!(result.unwrap_err().is::<interstitial::Blocked>());
    assert_eq!(obs[0].outcome, "blocked");
}
struct Wall;
#[async_trait]
impl Resolver for Wall {
    fn name(&self) -> &'static str {
        "web"
    }
    fn matches(&self, _: &str) -> bool {
        true
    }
    async fn resolve(&self, _: &str) -> Result<ResourceMeta> {
        Err(interstitial::Blocked.into())
    }
    fn refresh_after(&self, d: &ResourceMeta) -> std::time::Duration {
        std::time::Duration::from_secs(d.ttl_secs)
    }
}
#[sqlx::test(migrations = "../../migrations")]
async fn blocked_jobs_clear_cards_keep_events_and_do_not_confirm_disappearance(pool: PgPool) {
    let uri = "https://public.example/event";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.set_type("Event");
    d.name = Some("Accepted event".into());
    d.start = Some(chrono::Utc::now() + chrono::Duration::days(2));
    pinakes_core::group_keys::add(&mut d, "canonical", uri);
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let cfg = QueueConfig {
        blocked_ttl_secs: 172800,
        ..Default::default()
    };
    for _ in 0..6 {
        queue::run_job(
            &pool,
            &Wall,
            &cfg,
            &queue::Job {
                id: id.clone(),
                uri: uri.into(),
                generation: 0,
                attempts: 0,
            },
        )
        .await
        .unwrap();
        let (status,dead,name,attempts,delay,keys,start):(String,bool,Option<String>,i32,f64,i64,bool)=sqlx::query_as("SELECT status,dead_letter,name,attempts,extract(epoch FROM(next_attempt_at-now()))::float8,(SELECT count(*) FROM resource_group WHERE resource_id=$1),start_at IS NOT NULL FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(status, "blocked");
        assert!(!dead && name.is_none() && attempts == 0 && keys == 0 && start);
        assert!(delay > 172790.0);
    }
    let cancelled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resource_change WHERE resource_id=$1 AND class='cancelled'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cancelled, 0);
    let metrics = telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_blocked_resources{host=\"public.example\"} 1"));
    d.status = Status::Missing;
    for expected in ["blocked", "missing"] {
        d.fetched_at += 1;
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, expected);
    }
    d.status = Status::Ok;
    d.fetched_at += 1;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ok");
}

#[test]
fn main_entity_canonical_is_identity_but_publisher_nodes_are_not() {
    let uri = "https://example.org/article";
    let f = page(
        uri,
        r#"<script type="application/ld+json">{"@type":"WebPage","url":"https://example.org/wrapper","mainEntity":{"@type":"Article","url":"https://example.org/canonical","name":"Main"}}</script>"#,
        200,
    );
    let d = web::extract(uri, &f, 86400).doc;
    assert!(d
        .group_keys
        .iter()
        .any(|k| k.target.as_deref() == Some("https://example.org/canonical")));
    assert!(!d
        .group_keys
        .iter()
        .any(|k| k.target.as_deref() == Some("https://example.org/wrapper")));
}

#[test]
fn round4_wall_heuristics_need_context_and_cover_more_vendors() {
    let uri = "https://app.example/dashboard";
    let at = |body: &str, status: u16| {
        interstitial::inspect(&uri.parse().unwrap(), &page(uri, body, status))
    };
    let long = "Useful article text. ".repeat(120);
    // V12: a client-rendered shell's <noscript> notice is not visible text.
    let shell = "<title>Dashboard</title><meta property='og:title' content='Dashboard'><body><noscript>You need to enable JavaScript to run this app.</noscript><div id='root'></div></body>";
    assert_eq!(at(shell, 200), Page::Content);
    assert_eq!(
        web::extract(uri, &page(uri, shell, 200), 86400)
            .doc
            .name
            .as_deref(),
        Some("Dashboard")
    );
    // A captcha in a comment form of a full page is content; on a short or 403 page it is a wall.
    let comments = format!(
        "<title>Post</title><body>{long}<form><div class='g-recaptcha'></div></form></body>"
    );
    assert_eq!(at(&comments, 200), Page::Content);
    assert_eq!(at(&comments, 403), Page::Blocked);
    assert_eq!(
        at("<body><div class='g-recaptcha'></div></body>", 200),
        Page::Blocked
    );
    // An auto-reload (no URL), a slow self-refresh and a slow refresh of a full page are content.
    assert_eq!(
        at(
            &format!("<head><meta http-equiv='refresh' content='300'></head><body>{long}</body>"),
            200
        ),
        Page::Content
    );
    assert_eq!(
        at(
            "<head><meta http-equiv='refresh' content='300'></head><body>Live score</body>",
            200
        ),
        Page::Content
    );
    assert_eq!(
        at(&format!("<head><meta http-equiv='refresh' content='600; url={uri}'></head><body>Ticker</body>"), 200),
        Page::Content
    );
    assert_eq!(
        at(&format!("<head><meta http-equiv='refresh' content='900;url=/home'></head><body>{long}</body>"), 200),
        Page::Content
    );
    for content in [
        "0; url=/next",
        "0;URL='/next'",
        "2, /next",
        "0; url = '/next'",
    ] {
        assert_eq!(
            at(
                &format!("<head><meta http-equiv='refresh' content=\"{content}\"></head>"),
                200
            ),
            Page::Redirect("/next".into()),
            "{content}"
        );
    }
    // A long page titled like a marker is content; the same title on a challenge is a wall.
    let film = format!(
        "<title>Just a Moment (2026 film)</title><body><h1>Just a Moment</h1>{long}</body>"
    );
    assert_eq!(at(&film, 200), Page::Content);
    assert_eq!(at(&film, 503), Page::Blocked);
    // V16: more vendor walls.
    for body in [
        "<title>bloomberg.com</title><body><h1>Are you a robot?</h1></body>",
        "<body>Request unsuccessful. Incapsula incident ID: 123-456</body>",
        "<body>Please enable JS and disable any ad blocker</body>",
        "<body><iframe src='https://geo.captcha-delivery.com/captcha/?initialCid=x'></iframe></body>",
        "<body><div id='px-captcha'></div>Press &amp; Hold</body>",
    ] {
        assert_eq!(at(body, 200), Page::Blocked, "{body}");
    }
    let requested: Url = "https://www.linkedin.com/posts/someone_activity-1"
        .parse()
        .unwrap();
    for wall in [
        "https://www.linkedin.com/authwall?trk=gf&sessionRedirect=x",
        "https://www.facebook.com/login/?next=https%3A%2F%2Fwww.facebook.com%2Fevents%2F1",
        "https://example.com/signin",
        "https://www.instagram.com/accounts/login/",
    ] {
        let f = page(
            wall,
            &format!("<title>Sign in</title><body>{long}</body>"),
            200,
        );
        assert_eq!(
            interstitial::inspect(&requested, &f),
            Page::Blocked,
            "{wall}"
        );
    }
    // Tagging the sign-in page itself, or a path merely starting with "login", is content.
    for own in ["https://example.com/signin/", "https://example.com/loginfo"] {
        let f = page(
            own,
            &format!("<title>Sign in</title><body>{long}</body>"),
            200,
        );
        let requested: Url = own.trim_end_matches('/').parse().unwrap();
        assert_eq!(
            interstitial::inspect(&requested, &f),
            Page::Content,
            "{own}"
        );
    }
}

#[test]
fn round4_wall_overrides_and_marker_files_are_validated() {
    use interstitial::{WallOverride, Walls};
    use std::collections::BTreeMap;
    let uri = "https://news.example.org/a";
    let wall = page(uri, "<body>Access denied</body>", 200);
    let content = page(uri, &"Fine article. ".repeat(300), 200);
    let walls = Walls::load(
        None,
        &BTreeMap::from([
            ("example.org".to_string(), WallOverride::Content),
            ("blocked.example".to_string(), WallOverride::Blocked),
        ]),
    )
    .unwrap();
    // Longest-suffix host match; a content override still follows redirect stubs.
    assert_eq!(
        interstitial::inspect_with(&wall.url, &wall, &walls),
        Page::Content
    );
    let stub = page(uri, "<title>Redirecting to: /b</title>", 200);
    assert_eq!(
        interstitial::inspect_with(&stub.url, &stub, &walls),
        Page::Redirect("/b".into())
    );
    let blocked = page(
        "https://www.blocked.example/a",
        &"Fine article. ".repeat(300),
        200,
    );
    assert_eq!(
        interstitial::inspect_with(&blocked.url, &blocked, &walls),
        Page::Blocked
    );
    assert_eq!(
        interstitial::inspect_with(&content.url, &content, &Walls::default()),
        Page::Content
    );
    // Replacement files are validated like the built-in one.
    let dir = std::env::temp_dir().join(format!("pinakes-walls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, body, ok) in [
        (
            "good.json",
            r#"{"text":["custom wall"],"captcha_selectors":[".x"],"wall_host_prefixes":["gate."],"wall_paths":["/gate"]}"#,
            true,
        ),
        (
            "selector.json",
            r#"{"text":["a"],"captcha_selectors":["[["],"wall_host_prefixes":[],"wall_paths":[]}"#,
            false,
        ),
        (
            "case.json",
            r#"{"text":["Upper"],"captcha_selectors":[],"wall_host_prefixes":[],"wall_paths":[]}"#,
            false,
        ),
        (
            "path.json",
            r#"{"text":["a"],"captcha_selectors":[],"wall_host_prefixes":[],"wall_paths":["login"]}"#,
            false,
        ),
        (
            "host.json",
            r#"{"text":["a"],"captcha_selectors":[],"wall_host_prefixes":[],"wall_paths":[],"overrides":{"Bad Host":"content"}}"#,
            false,
        ),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let loaded = Walls::load(path.to_str(), &BTreeMap::new());
        assert_eq!(loaded.is_ok(), ok, "{name}");
        if ok {
            let custom = loaded.unwrap();
            let f = page(uri, "<body>Custom wall</body>", 200);
            assert_eq!(
                interstitial::inspect_with(&f.url, &f, &custom),
                Page::Blocked
            );
            assert_eq!(
                interstitial::inspect_with(&wall.url, &wall, &custom),
                Page::Content
            );
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    let mut cfg = pinakes_core::config::Config::default();
    cfg.web
        .wall_overrides
        .insert("Bad".into(), WallOverride::Content);
    assert!(cfg.validate().is_err());
    let mut cfg = pinakes_core::config::Config::default();
    cfg.pubky
        .wall_overrides
        .insert("example.org".into(), WallOverride::Content);
    assert!(cfg.validate().is_err());
}
