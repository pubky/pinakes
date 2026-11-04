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
        assert_eq!(
            interstitial::inspect(&requested, &f).unwrap(),
            Page::Blocked
        );
        let d = web::extract(requested.as_str(), &f, 86400).unwrap().doc;
        assert_eq!(d.status, Status::Unreachable);
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
        interstitial::inspect(&uri.parse().unwrap(), &f).unwrap(),
        Page::Redirect("/pkarr/".into())
    );
    for body in [
        "<script>window.location.replace('/next')</script>",
        "<title>Redirecting to: /next</title>",
        "<body>Redirecting to <a href='/next'>next</a></body>",
    ] {
        assert_eq!(
            interstitial::inspect(&f.url, &page(uri, body, 200)).unwrap(),
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
            interstitial::inspect(&f.url, &page(uri, &format!("<body>{marker}</body>"), 200))
                .unwrap(),
            Page::Blocked
        );
    }
    let ordinary=format!("<title>A real article</title><body>{}<script>const message='Enable JavaScript';</script></body>","Useful text. ".repeat(300));
    assert_eq!(
        interstitial::inspect(&f.url, &page(uri, &ordinary, 200)).unwrap(),
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
        assert_eq!(
            pinakes_core::resolver::html::off_runtime(|| interstitial::inspect(&uri, &f).unwrap()),
            expected
        );
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

#[test]
fn main_entity_canonical_is_identity_but_publisher_nodes_are_not() {
    let uri = "https://example.org/article";
    let f = page(
        uri,
        r#"<script type="application/ld+json">{"@type":"WebPage","url":"https://example.org/wrapper","mainEntity":{"@type":"Article","url":"https://example.org/canonical","name":"Main"}}</script>"#,
        200,
    );
    let d = web::extract(uri, &f, 86400).unwrap().doc;
    assert!(d
        .aliases
        .contains(&"https://example.org/canonical".to_owned()));
    assert!(!d
        .aliases
        .contains(&"https://example.org/wrapper".to_owned()));
}

#[test]
fn wall_heuristics_need_context_and_cover_more_vendors() {
    let uri = "https://app.example/dashboard";
    let at = |body: &str, status: u16| {
        interstitial::inspect(&uri.parse().unwrap(), &page(uri, body, status)).unwrap()
    };
    let long = "Useful article text. ".repeat(120);
    // A client-rendered shell's <noscript> notice is not visible text.
    let shell = "<title>Dashboard</title><meta property='og:title' content='Dashboard'><body><noscript>You need to enable JavaScript to run this app.</noscript><div id='root'></div></body>";
    assert_eq!(at(shell, 200), Page::Content);
    assert_eq!(
        web::extract(uri, &page(uri, shell, 200), 86400)
            .unwrap()
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
    // More vendor walls.
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
            interstitial::inspect(&requested, &f).unwrap(),
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
            interstitial::inspect(&requested, &f).unwrap(),
            Page::Content,
            "{own}"
        );
    }
}

/// A challenge page is a failed fetch: the resource turns `unreachable` with the normal backoff
/// and dead letter, keeps its accepted facts and group keys, never confirms disappearance and
/// writes no change line; it stays in search and recovers with the next good page.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn walls_are_unreachable_with_normal_backoff_and_keep_their_facts(pool: PgPool) {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let uri = "https://public.example/event";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.set_type("Event");
    d.name = Some("Accepted event".into());
    d.start = Some(chrono::Utc::now() + chrono::Duration::days(2));
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','news','/pub/app/tags/n',now())").bind(&id).bind("a".repeat(52)).execute(&pool).await.unwrap();
    let cfg = QueueConfig::default();
    let mut delays = vec![];
    for attempt in 1..=cfg.max_failures as i32 {
        let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM resource WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let job = queue::Job {
            id: id.clone(),
            uri: uri.into(),
            generation: 0,
            attempts,
        };
        queue::run_job(&pool, &Wall, &cfg, &job).await.unwrap();
        let (status, dead, name, attempts, delay, error, start): (String, bool, Option<String>, i32, f64, String, bool) = sqlx::query_as("SELECT status,dead_letter,name,attempts,extract(epoch FROM(next_attempt_at-now()))::float8,last_error,start_at IS NOT NULL FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!((status.as_str(), error.as_str()), ("unreachable", "wall"));
        assert_eq!(attempts, attempt);
        assert_eq!(dead, attempt == cfg.max_failures as i32);
        assert!(name.as_deref() == Some("Accepted event") && start);
        delays.push(delay);
    }
    assert!(delays.windows(2).all(|w| w[1] > w[0]), "{delays:?}");
    let classes: Vec<String> =
        sqlx::query_scalar("SELECT class FROM resource_change WHERE resource_id=$1 ORDER BY seq")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(classes, vec!["created".to_string()]);
    let app = pinakes_core::api::router(
        pinakes_core::api::AppState::new(pool.clone(), Default::default()).unwrap(),
    );
    let get = |path: String| {
        let app = app.clone();
        async move {
            let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(
                "127.0.0.2:1234".parse::<SocketAddr>().unwrap(),
            ));
            let response = app.oneshot(request).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        }
    };
    let found = get("/v1/search?tags=news".into()).await;
    assert_eq!(found["items"][0]["id"], id.as_str(), "{found}");
    assert_eq!(found["items"][0]["status"], "unreachable");
    assert_eq!(found["items"][0]["name"], "Accepted event");
    d.fetched_at += 1;
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    assert_eq!(get(format!("/v1/resources/{id}")).await["status"], "ok");
}
#[test]
fn multi_byte_refresh_content_and_sign_in_flows_are_content() {
    // Meta-refresh content is page text: multi-byte characters at any offset never panic,
    // and every page still classifies.
    let requested = Url::parse("https://example.org/a").unwrap();
    let long = "Article text. ".repeat(200);
    let mut seed = 0x2545_f491_u64;
    let parts = [
        "5", ";", ",", " ", "url", "=", "'", "€", "é", "😀", "/x", "ü",
    ];
    for n in 0..2000 {
        let content: String = (0..1 + n % 7)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                parts[(seed % parts.len() as u64) as usize]
            })
            .collect();
        let content = content.replace('"', "");
        for body in [String::new(), long.clone()] {
            let f = page(
                "https://example.org/a",
                &format!(r#"<meta http-equiv="refresh" content="{content}"><body>{body}</body>"#),
                200,
            );
            let _ = interstitial::inspect(&requested, &f).unwrap();
        }
    }
    let f = page(
        "https://example.org/a",
        &format!(r#"<meta http-equiv="refresh" content="5; a€b"><body>{long}</body>"#),
        200,
    );
    assert_eq!(
        interstitial::inspect(&requested, &f).unwrap(),
        Page::Redirect("a€b".into())
    );

    // Case or trailing-slash normalisation is not a sign-in redirect, and a sign-in page
    // continuing its own flow is the page that was asked for.
    for (asked, landed) in [
        ("https://example.com/Login", "https://example.com/login"),
        ("https://example.com/SignIn/", "https://example.com/signin"),
        (
            "https://example.com/signin",
            "https://example.com/signin/callback",
        ),
        (
            "https://example.com/login",
            "https://example.com/accounts/login",
        ),
    ] {
        let f = page(
            landed,
            &format!("<title>Sign in</title><body>{long}</body>"),
            200,
        );
        assert_eq!(
            interstitial::inspect(&asked.parse().unwrap(), &f).unwrap(),
            Page::Content,
            "{asked} -> {landed}"
        );
    }
    // A content page sent to a sign-in path is still a wall, whatever the path's case.
    for landed in [
        "https://example.com/LOGIN?next=/a",
        "https://example.com/signin/",
    ] {
        let f = page(
            landed,
            &format!("<title>Sign in</title><body>{long}</body>"),
            200,
        );
        assert_eq!(
            interstitial::inspect(&"https://example.com/posts/1".parse().unwrap(), &f).unwrap(),
            Page::Blocked,
            "{landed}"
        );
    }
}

/// The web route's own JSON-LD and microformat dates take the stored-instant range
/// (UTC years 0001–9999) like the event parsers.
#[test]
fn web_route_dates_are_bounded() {
    let uri = "https://example.org/talk";
    for (start, kept) in [
        ("2026-10-20T19:00:00Z", true),
        ("0000-01-01T00:00:00Z", false),
        ("9999-12-31T23:59:59-05:00", false),
    ] {
        let jsonld = format!(
            r#"<script type="application/ld+json">{{"@type":"Article","name":"Talk","startDate":"{start}"}}</script>"#
        );
        let microformat = format!(
            r#"<div class="h-event"><b class="p-name">Talk</b><time class="dt-start" datetime="{start}"></time></div>"#
        );
        for body in [jsonld, microformat] {
            let d = web::extract(uri, &page(uri, &body, 200), 86400)
                .unwrap()
                .doc;
            assert_eq!(d.start.is_some(), kept, "{body}");
        }
    }
}

/// A challenge or sign-in host is a wall only when a redirect lands there from another host of
/// the site; a page referenced on that host itself is content.
#[test]
fn wall_hosts_need_a_redirect_from_another_host() {
    let body = "<title>Help centre</title><p>How to reset your password and other answers.</p>";
    let mut landed = page("https://login.example.org/help", body, 200);
    for requested in [
        "https://login.example.org/help",
        "https://challenge.example.org/about",
    ] {
        landed.url = Url::parse(requested).unwrap();
        let requested = Url::parse(requested).unwrap();
        assert_eq!(
            interstitial::inspect(&requested, &landed).unwrap(),
            Page::Content,
            "{requested}"
        );
    }
    landed.url = Url::parse("https://login.example.org/help").unwrap();
    let requested = Url::parse("https://www.example.org/article").unwrap();
    assert_eq!(
        interstitial::inspect(&requested, &landed).unwrap(),
        Page::Blocked
    );
}

/// A page whose JSON-LD has a plain `&`.
struct Wallet;
#[async_trait]
impl Transport for Wallet {
    async fn get(&self, u: &Url, _: &[SocketAddr]) -> Result<Fetched> {
        Ok(page(
            u.as_str(),
            r#"<title>Wallet</title><script type="application/ld+json">{"@type":"WebPage","name":"Money transfers & online payments","url":"https://wallet.example/?id=5&currency=EUR"}</script>"#,
            200,
        ))
    }
}
/// Text stored before the decoding fix (a plain `&` read back as `&amp;`) is repaired by
/// `reextract web`: the resource is fetched again and extracted with today's rules.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn reextract_repairs_text_stored_before_the_decoding_fix(pool: PgPool) {
    let uri = "https://wallet.example/";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut old = ResourceMeta::new(uri, "web", 86400);
    old.set_type("WebPage");
    old.name = Some("Money transfers &amp; online payments".into());
    store::save_document(&pool, &id, 0, &old).await.unwrap();
    assert_eq!(queue::reextract(&pool, &["web".into()]).await.unwrap(), 1);
    let cfg = QueueConfig::default();
    let job = queue::claim(&pool, "web", &cfg)
        .await
        .unwrap()
        .expect("re-queued");
    let resolver = web::WebResolver {
        oembed_policy: Default::default(),
        cards: None,
        events: None,
        config: Default::default(),
        egress: Egress::with_backends(
            EgressConfig {
                host_requests_per_second: 1000.0,
                ..Default::default()
            },
            Arc::new(PublicDns),
            Arc::new(Wallet),
        ),
    };
    queue::run_job(&pool, &resolver, &cfg, &job).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Money transfers & online payments");
}
