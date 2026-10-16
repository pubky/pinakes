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
