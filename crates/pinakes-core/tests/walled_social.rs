//! A social site that refuses Pinakes: what its links' cards show (`embed` from the link alone),
//! when they are asked again (a refusal's backoff, a host that refuses the whole address) and
//! what stays of a card read before. Instagram's links, from scripted answers; each test returns
//! early in a build without the `instagram` source.
use anyhow::Result;
use async_trait::async_trait;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    queue,
    resolver::{
        cards::CardResolver,
        egress::{Egress, Fetched, Transport},
    },
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use url::Url;
mod common;
use common::PublicDns;

/// How the scripted site answers a path.
#[derive(Clone, Copy)]
enum Answer {
    /// A post's own page, with its Open Graph tags: `Someone on Instagram: "A caption"`.
    Post,
    /// A page without the object's tags, as Instagram answers a removed or private post.
    Tagless,
    /// The sign-in redirect for a page (`/accounts/login/?next=…`).
    SignIn,
    /// The sign-in redirect that says the site refuses the whole address (`&is_from_rle`).
    RateLimited,
    /// 404.
    Gone,
}

/// The site, by path, recording every URL asked.
#[derive(Default)]
struct Site {
    answers: Mutex<BTreeMap<String, Answer>>,
    asked: Mutex<Vec<String>>,
}
impl Site {
    fn answer(&self, path: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(path.into(), answer);
    }
    fn asked(&self) -> Vec<String> {
        std::mem::take(&mut *self.asked.lock().unwrap())
    }
}
#[async_trait]
impl Transport for Site {
    async fn get(&self, url: &Url, _: &[SocketAddr]) -> Result<Fetched> {
        self.asked.lock().unwrap().push(url.to_string());
        let html = |status: u16, body: String| {
            common::fetched(url.clone(), status, "text/html".into(), body.into_bytes())
        };
        if url.path() == "/accounts/login/" {
            return Ok(html(
                200,
                "<html><head><title>Login • Instagram</title></head><body>Log in</body></html>"
                    .into(),
            ));
        }
        let redirect = |marker: &str| Fetched {
            location: Some(format!(
                "/accounts/login/?next=https%3A%2F%2Fwww.instagram.com{}{marker}",
                url.path()
            )),
            ..html(302, String::new())
        };
        let answer = self.answers.lock().unwrap().get(url.path()).copied();
        Ok(match answer {
            Some(Answer::Post) => html(
                200,
                format!(
                    "<html><head><title>Instagram</title>\
                     <meta property=\"og:title\" content=\"Someone on Instagram: &quot;A caption&quot;\">\
                     <meta property=\"og:description\" content=\"12 likes - someone\">\
                     <meta property=\"og:image\" content=\"https://scontent.cdninstagram.com/v/a.jpg\">\
                     <meta property=\"og:url\" content=\"https://www.instagram.com{}\">\
                     </head><body>A caption</body></html>",
                    url.path()
                ),
            ),
            Some(Answer::Tagless) => html(
                200,
                "<html><head><title>Instagram</title></head><body><a href=\"/\">Instagram</a> \
                 Photos and videos from friends.</body></html>"
                    .into(),
            ),
            Some(Answer::SignIn) => redirect(""),
            Some(Answer::RateLimited) => redirect("&is_from_rle"),
            Some(Answer::Gone) => html(404, "<html><title>Not found</title></html>".into()),
            None => panic!("unscripted request {url}"),
        })
    }
}

fn has_instagram() -> bool {
    pinakes_core::sources::by_name("instagram").is_some()
}
fn official(site: &Arc<Site>) -> CardResolver {
    CardResolver::new(
        "official",
        Arc::new(Config::default()),
        Egress::with_backends(
            common::unpaced_egress_config(),
            Arc::new(PublicDns),
            site.clone(),
        ),
    )
}
async fn read(pool: &PgPool, resolver: &CardResolver, uri: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(pool)
        .await
        .unwrap();
    let job = queue::Job {
        id: id.clone(),
        uri: uri.into(),
        attempts: 0,
        generation,
    };
    queue::run_job(pool, resolver, &Default::default(), &job)
        .await
        .unwrap();
    id
}
/// What the queue holds for a link: its status, card name, `blocked` fact, `last_error`,
/// `timeout_streak`, attempts, and the seconds from now to its next read.
#[derive(Debug, PartialEq)]
struct Row {
    status: String,
    name: Option<String>,
    blocked: Option<String>,
    last_error: Option<String>,
    streak: i32,
    attempts: i32,
    dead_letter: bool,
    next_in: f64,
    /// Seconds from now to `refresh_at` (none: `None`).
    refresh_in: Option<f64>,
}
async fn row(pool: &PgPool, id: &str) -> Row {
    let (status, name, blocked, last_error, streak, attempts, dead_letter, next_in, refresh_in) =
        sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>, i32, i32, bool, f64, Option<f64>)>(
            "SELECT status, doc->>'name', doc->'attributes'->>'blocked', last_error, timeout_streak, attempts, dead_letter, \
             extract(epoch FROM next_attempt_at-now())::float8, extract(epoch FROM refresh_at-now())::float8 \
             FROM resource WHERE id=$1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    Row {
        status,
        name,
        blocked,
        last_error,
        streak,
        attempts,
        dead_letter,
        next_in,
        refresh_in,
    }
}
/// `seconds` from now, give or take a minute.
fn about(next_in: f64, seconds: u64) -> bool {
    (next_in - seconds as f64).abs() < 60.0
}
fn app(pool: &PgPool) -> axum::Router {
    api::router(AppState::new(pool.clone(), Config::default()).unwrap())
}

/// An Instagram post or reel card carries Instagram's own frame for its link, whatever its read
/// gave: a post read whole, the plain card of a tagless page, the card of a refused read, a
/// link not read yet, and a card another route stored (the source off). A profile has none.
#[sqlx::test(migrations = "../../migrations")]
async fn an_instagram_card_carries_its_frame_in_every_state(pool: PgPool) {
    if !has_instagram() {
        return;
    }
    let site = Arc::new(Site::default());
    site.answer("/p/AAAAAAread/", Answer::Post);
    site.answer("/p/AAAAAplain/", Answer::Tagless);
    site.answer("/reel/AAAAblocked/", Answer::SignIn);
    site.answer("/someone/", Answer::SignIn);
    let resolver = official(&site);
    let ids = [
        read(&pool, &resolver, "https://www.instagram.com/p/AAAAAAread/").await,
        read(&pool, &resolver, "https://www.instagram.com/p/AAAAAplain/").await,
        read(
            &pool,
            &resolver,
            "https://www.instagram.com/reel/AAAAblocked/",
        )
        .await,
        read(&pool, &resolver, "https://www.instagram.com/someone/").await,
        store::ensure_resource(&pool, "https://www.instagram.com/p/AAAApending/")
            .await
            .unwrap(),
    ];
    let web = "https://www.instagram.com/p/AAAAAAAweb/";
    let web_id = store::ensure_resource(&pool, web).await.unwrap();
    let mut doc = ResourceMeta::new(web, "web", 86400);
    doc.set_type("WebPage");
    doc.name = Some("Instagram".into());
    store::save_document(&pool, &web_id, 0, &doc).await.unwrap();

    let app = app(&pool);
    let card = |id: &str| {
        let app = app.clone();
        let path = format!("/v1/resources/{id}");
        async move { common::get(&app, &path).await }
    };
    let read_card = card(&ids[0]).await;
    assert_eq!(read_card["name"], "A caption", "{read_card}");
    let plain = card(&ids[1]).await;
    assert_eq!(plain["name"], "Post on instagram.com", "{plain}");
    let blocked = card(&ids[2]).await;
    assert_eq!(blocked["facts"]["blocked"], "true", "{blocked}");
    for (id, src) in [
        (
            &ids[0],
            "https://www.instagram.com/p/AAAAAAread/embed/captioned/",
        ),
        (
            &ids[1],
            "https://www.instagram.com/p/AAAAAplain/embed/captioned/",
        ),
        (
            &ids[2],
            "https://www.instagram.com/reel/AAAAblocked/embed/captioned/",
        ),
        (
            &ids[4],
            "https://www.instagram.com/p/AAAApending/embed/captioned/",
        ),
        (
            &web_id,
            "https://www.instagram.com/p/AAAAAAAweb/embed/captioned/",
        ),
    ] {
        let card = card(id).await;
        assert_eq!(
            card["embed"],
            serde_json::json!({"provider": "instagram", "kind": "post", "src": src,
                "height": null, "aspect": null, "allow": [], "autoplay_param": null}),
            "{card}"
        );
    }
    let profile = card(&ids[3]).await;
    assert_eq!(profile["type"], "Person", "{profile}");
    assert_eq!(profile["embed"], Value::Null, "{profile}");
}

/// A refused link is asked again a day later, then after 2, 4 and 7 days, every 7 days after
/// that: never charged, never dead-lettered, its plain refused card kept. A read that answers
/// ends the streak.
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_link_waits_a_day_doubling_to_a_week(pool: PgPool) {
    if !has_instagram() {
        return;
    }
    let site = Arc::new(Site::default());
    site.answer("/p/AAAArefused/", Answer::SignIn);
    let resolver = official(&site);
    let uri = "https://www.instagram.com/p/AAAArefused/";
    for days in [1, 2, 4, 7, 7] {
        let id = read(&pool, &resolver, uri).await;
        let row = row(&pool, &id).await;
        assert_eq!(
            (
                row.status.as_str(),
                row.name.as_deref(),
                row.blocked.as_deref(),
                row.last_error.as_deref(),
                row.attempts,
                row.dead_letter
            ),
            (
                "ok",
                Some("Post on instagram.com"),
                Some("true"),
                Some("wall"),
                0,
                false
            ),
            "{row:?}"
        );
        assert!(about(row.next_in, days * 86400), "{days} days: {row:?}");
    }
    assert_eq!(
        site.asked().len(),
        10,
        "each read: the post and the sign-in page"
    );
    site.answer("/p/AAAArefused/", Answer::Post);
    let id = read(&pool, &resolver, uri).await;
    let row = row(&pool, &id).await;
    assert_eq!(
        (
            row.name.as_deref(),
            row.blocked.as_deref(),
            row.last_error.as_deref(),
            row.streak
        ),
        (Some("A caption"), None, None, 0)
    );
    // Instagram's own refresh: the official route's day, not the hour of other social content.
    assert!((82_800.0..=86_460.0).contains(&row.next_in), "{row:?}");
}

/// A refusal on a refresh keeps the card read before, and the link waits the refusals' backoff
/// (a day, then two). A post found gone (404, confirmed by a second read) is still emptied, and
/// a refusal after that leaves it gone.
#[sqlx::test(migrations = "../../migrations")]
async fn a_refused_refresh_keeps_the_card_and_waits(pool: PgPool) {
    if !has_instagram() {
        return;
    }
    let site = Arc::new(Site::default());
    let path = "/p/AAAAAAgood/";
    let uri = "https://www.instagram.com/p/AAAAAAgood/";
    site.answer(path, Answer::Post);
    let resolver = official(&site);
    let id = read(&pool, &resolver, uri).await;
    assert_eq!(row(&pool, &id).await.name.as_deref(), Some("A caption"));
    site.answer(path, Answer::SignIn);
    for days in [1, 2] {
        read(&pool, &resolver, uri).await;
        let row = row(&pool, &id).await;
        assert_eq!(
            (
                row.status.as_str(),
                row.name.as_deref(),
                row.blocked.as_deref(),
                row.last_error.as_deref(),
                row.attempts
            ),
            ("ok", Some("A caption"), None, Some("wall"), 0),
            "{row:?}"
        );
        assert!(about(row.next_in, days * 86400), "{days} days: {row:?}");
        // Its refresh waits as long: the kept card is no overdue refresh meanwhile.
        assert!(
            row.refresh_in.is_some_and(|r| about(r, days * 86400)),
            "{days} days: {row:?}"
        );
    }
    site.answer(path, Answer::Gone);
    read(&pool, &resolver, uri).await;
    read(&pool, &resolver, uri).await;
    let (status, name, description): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, doc->>'name', doc->>'description' FROM resource WHERE id=$1",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), name, description),
        ("missing", None, None)
    );
    // A refusal after that says nothing about the post: it stays gone, with no frame.
    site.answer(path, Answer::SignIn);
    read(&pool, &resolver, uri).await;
    let gone = row(&pool, &id).await;
    assert_eq!(
        (
            gone.status.as_str(),
            gone.name.as_deref(),
            gone.blocked.as_deref(),
            gone.last_error.as_deref()
        ),
        ("missing", None, None, Some("wall")),
        "{gone:?}"
    );
    assert!(about(gone.next_in, 86400), "{gone:?}");
    let card = common::get(&app(&pool), &format!("/v1/resources/{id}")).await;
    assert_eq!(card["embed"], Value::Null, "{card}");
}

/// Instagram's sign-in redirect that says it refuses the whole address (`is_from_rle`) is the
/// link's refusal, read without its sign-in page, and pauses Instagram for a day: another link,
/// new or due, sends nothing meanwhile and waits (a refused link keeps its streak).
#[sqlx::test(migrations = "../../migrations")]
async fn a_rate_limited_address_pauses_the_whole_host(pool: PgPool) {
    if !has_instagram() {
        return;
    }
    let site = Arc::new(Site::default());
    site.answer("/p/AAAAAAAAbad/", Answer::SignIn);
    site.answer("/p/AAAAAAAflag/", Answer::RateLimited);
    site.answer("/p/AAAAAAAnext/", Answer::Post);
    let resolver = official(&site);
    // A link refused before, with its streak.
    let bad = read(&pool, &resolver, "https://www.instagram.com/p/AAAAAAAAbad/").await;
    assert_eq!(row(&pool, &bad).await.streak, 1);
    site.asked();

    let flag = read(&pool, &resolver, "https://www.instagram.com/p/AAAAAAAflag/").await;
    assert_eq!(
        site.asked(),
        ["https://www.instagram.com/p/AAAAAAAflag/"],
        "the sign-in page is not asked for"
    );
    let flagged = row(&pool, &flag).await;
    assert_eq!(
        (flagged.blocked.as_deref(), flagged.last_error.as_deref()),
        (Some("true"), Some("wall"))
    );
    assert!(about(flagged.next_in, 86400), "{flagged:?}");

    let next = read(&pool, &resolver, "https://www.instagram.com/p/AAAAAAAnext/").await;
    read(&pool, &resolver, "https://www.instagram.com/p/AAAAAAAAbad/").await;
    assert!(
        site.asked().is_empty(),
        "nothing is sent while the host is paused"
    );
    let waiting = row(&pool, &next).await;
    assert_eq!(
        (
            waiting.status.as_str(),
            waiting.last_error.as_deref(),
            waiting.attempts
        ),
        ("pending", Some("rate_limited"), 0),
        "{waiting:?}"
    );
    assert!(waiting.next_in <= 3601.0, "{waiting:?}");
    let refused = row(&pool, &bad).await;
    assert_eq!(
        (
            refused.last_error.as_deref(),
            refused.streak,
            refused.attempts
        ),
        (Some("wall"), 1, 0),
        "{refused:?}"
    );
}

/// A link found gone from its first reads (two 404s: it never had a card that said more) stays
/// gone through a later refusal: no "Blocked by the site." card, and no frame.
#[sqlx::test(migrations = "../../migrations")]
async fn a_link_gone_from_the_start_stays_gone_through_a_refusal(pool: PgPool) {
    if !has_instagram() {
        return;
    }
    let site = Arc::new(Site::default());
    let path = "/p/AAAAAnever/";
    let uri = "https://www.instagram.com/p/AAAAAnever/";
    site.answer(path, Answer::Gone);
    let resolver = official(&site);
    let id = read(&pool, &resolver, uri).await;
    read(&pool, &resolver, uri).await;
    let ever: bool = sqlx::query_scalar("SELECT ever_existed FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!ever);
    assert_eq!(row(&pool, &id).await.status, "missing");
    site.answer(path, Answer::SignIn);
    read(&pool, &resolver, uri).await;
    let gone = row(&pool, &id).await;
    assert_eq!(
        (
            gone.status.as_str(),
            gone.blocked.as_deref(),
            gone.last_error.as_deref()
        ),
        ("missing", None, Some("wall")),
        "{gone:?}"
    );
    let card = common::get(&app(&pool), &format!("/v1/resources/{id}")).await;
    assert_eq!(card["status"], "missing", "{card}");
    assert_eq!(card["embed"], Value::Null, "{card}");
}
