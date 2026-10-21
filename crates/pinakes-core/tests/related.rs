//! `GET /v1/search?related={id}`: what the people who tagged a resource also referenced.
use axum::{body::Body, extract::ConnectInfo, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
mod common;

/// A public key of 52 z-base-32 characters, one per tagger.
fn author(n: usize) -> String {
    let alphabet = b"ybndrfg8ejkmcpqxot1uwisza345h769";
    (0..52)
        .map(|i| alphabet[(n * 7 + i) % alphabet.len()] as char)
        .collect()
}

async fn save(pool: &PgPool, uri: &str, kind: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut doc = ResourceMeta::new(uri, "web", 86400);
    doc.set_type(kind);
    doc.name = Some(uri.rsplit('/').next().unwrap().into());
    store::save_document(pool, &id, 0, &doc).await.unwrap();
    id
}

/// `who` tags `id`, `days` ago.
async fn tag(pool: &PgPool, id: &str, who: usize, days: i64) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','music',$3,now()-$4*interval '1 day')")
        .bind(id)
        .bind(author(who))
        .bind(format!("/pub/pubky.app/tags/{id}-{who}"))
        .bind(days as f64)
        .execute(pool)
        .await
        .unwrap();
}

async fn get_with(config: &Config, pool: &PgPool, path: &str) -> (u16, Value) {
    let app = api::router(AppState::new(pool.clone(), config.clone()).unwrap());
    let mut request = Request::get(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 200 {
        common::check_response("GET", path, status, &value);
    }
    (status, value)
}
async fn related(pool: &PgPool, id: &str, query: &str) -> Value {
    let (status, value) = get_with(
        &Config::default(),
        pool,
        &format!("/v1/search?related={id}{query}"),
    )
    .await;
    assert_eq!(status, 200, "{value}");
    value
}
/// The rows' IDs, in order.
fn rows(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Put `members` in one curated group.
async fn group(pool: &PgPool, id: &str, members: &[&String]) {
    sqlx::query("INSERT INTO \"group\"(id,curated) VALUES($1,true)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE unit_id=ANY($2)")
        .bind(id)
        .bind(members.iter().map(|m| m.to_string()).collect::<Vec<_>>())
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn ranked_by_shared_taggers_then_recency(pool: PgPool) {
    let seed = save(&pool, "https://example.org/ok-computer", "MusicAlbum").await;
    let two = save(&pool, "https://example.org/kid-a", "MusicAlbum").await;
    let old = save(&pool, "https://example.org/the-bends", "MusicAlbum").await;
    let new = save(&pool, "https://example.org/in-rainbows", "MusicRecording").await;
    let nobody = save(&pool, "https://example.org/unrelated", "MusicAlbum").await;
    for who in [1, 2, 3] {
        tag(&pool, &seed, who, 1).await;
    }
    tag(&pool, &two, 1, 5).await;
    tag(&pool, &two, 2, 5).await;
    tag(&pool, &old, 1, 30).await;
    tag(&pool, &new, 3, 2).await;
    tag(&pool, &nobody, 9, 1).await;
    let v = related(&pool, &seed, "").await;
    assert_eq!(rows(&v), [two.clone(), new.clone(), old.clone()]);
    // It composes with the other filters: type (ancestors included), limit, a window, facets.
    assert_eq!(
        rows(&related(&pool, &seed, "&type=MusicAlbum").await),
        [two.clone(), old.clone()]
    );
    assert_eq!(
        rows(&related(&pool, &seed, "&limit=1").await),
        std::slice::from_ref(&two)
    );
    assert_eq!(
        rows(&related(&pool, &seed, "&window=7d").await),
        [two.clone(), new.clone()]
    );
    let faceted = related(&pool, &seed, "&facet=type").await;
    assert_eq!(
        faceted["facets"]["type"],
        serde_json::json!([{"type":"MusicAlbum","count":2},{"type":"MusicRecording","count":1}]),
        "facets count the related rows"
    );
    // A page and the next one keep the ranking (one daemon: cursors are its own).
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let page = |path: String| {
        let app = app.clone();
        async move {
            let mut request = Request::get(path).body(Body::empty()).unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
            ));
            let response = app.oneshot(request).await.unwrap();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    let first = page(format!("/v1/search?related={seed}&limit=2")).await;
    assert_eq!(rows(&first), [two.clone(), new.clone()]);
    let cursor: String =
        url::form_urlencoded::byte_serialize(first["cursor"].as_str().unwrap().as_bytes())
            .collect();
    let next = page(format!("/v1/search?related={seed}&limit=2&cursor={cursor}")).await;
    assert_eq!(rows(&next), std::slice::from_ref(&old));
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_group_counts_its_members_taggers_together_and_never_lists_itself(pool: PgPool) {
    // OK Computer on two services, one group; Kid A on two services, another; Amnesiac alone.
    let a = save(&pool, "https://example.org/ok-computer-tidal", "MusicAlbum").await;
    let b = save(
        &pool,
        "https://example.org/ok-computer-spotify",
        "MusicAlbum",
    )
    .await;
    let c = save(&pool, "https://example.org/kid-a-tidal", "MusicAlbum").await;
    let d = save(&pool, "https://example.org/kid-a-spotify", "MusicAlbum").await;
    let e = save(&pool, "https://example.org/amnesiac", "MusicAlbum").await;
    group(&pool, "0123456789abcdef0123456789abcdef", &[&a, &b]).await;
    group(&pool, "fedcba9876543210fedcba9876543210", &[&c, &d]).await;
    // The seed group's taggers are its members' (1 on a, 2 on b).
    tag(&pool, &a, 1, 1).await;
    tag(&pool, &b, 2, 1).await;
    tag(&pool, &c, 1, 2).await;
    tag(&pool, &d, 2, 3).await;
    tag(&pool, &e, 1, 1).await;
    tag(&pool, &e, 2, 4).await;
    for seed in [&a, &b] {
        // Without collapse: each resource by its own shared taggers; never the seed's group.
        let v = related(&pool, seed, "").await;
        assert_eq!(rows(&v), [e.clone(), c.clone(), d.clone()], "{v}");
        // Collapsed: one row per group, its members' taggers together. Kid A now has two, as
        // Amnesiac does, whose newest reference is newer.
        let v = related(&pool, seed, "&collapse=group").await;
        let got = rows(&v);
        assert_eq!(got.len(), 2, "{v}");
        assert_eq!(got[0], e);
        assert!(got[1] == c || got[1] == d, "{v}");
        assert_eq!(
            v["items"][1]["group"]["id"],
            "fedcba9876543210fedcba9876543210"
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn opted_out_old_and_unsampled_references_do_not_count(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicRecording").await;
    let post = "https://bsky.app/profile/x/post/1";
    let opted = store::ensure_resource(&pool, post).await.unwrap();
    let mut doc = ResourceMeta::new(post, "bluesky", 86400);
    doc.set_type("SocialMediaPosting");
    doc.attributes.insert("opted_out".into(), "true".into());
    store::save_document(&pool, &opted, 0, &doc).await.unwrap();
    let stale = save(&pool, "https://example.org/stale", "MusicRecording").await;
    let fresh = save(&pool, "https://example.org/fresh", "MusicRecording").await;
    tag(&pool, &seed, 1, 1).await;
    tag(&pool, &seed, 2, 2).await;
    // An old tagger of the seed, and an old reference of a recent tagger, are out of the window.
    tag(&pool, &seed, 3, 400).await;
    tag(&pool, &opted, 1, 1).await;
    tag(&pool, &stale, 1, 400).await;
    tag(&pool, &fresh, 2, 1).await;
    tag(&pool, &fresh, 3, 1).await;
    assert_eq!(
        rows(&related(&pool, &seed, "").await),
        std::slice::from_ref(&fresh)
    );
    // Only the most recent tagger with `limits.related_taggers = 1`: tagger 1, whose other
    // references are excluded.
    let mut one = Config::default();
    one.search.related_taggers = 1;
    let (_, v) = get_with(&one, &pool, &format!("/v1/search?related={seed}")).await;
    assert!(rows(&v).is_empty(), "{v}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_alias_reads_as_its_root_and_bad_requests_are_refused(pool: PgPool) {
    let root = save(&pool, "https://example.org/root", "MusicAlbum").await;
    let other = save(&pool, "https://example.org/other", "MusicAlbum").await;
    let alias = store::ensure_resource(&pool, "https://example.org/alias")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&alias)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    tag(&pool, &root, 1, 1).await;
    tag(&pool, &other, 1, 1).await;
    assert_eq!(rows(&related(&pool, &alias, "").await), [other]);
    // An unknown resource relates to nothing.
    assert!(rows(&related(&pool, &"0".repeat(32), "").await).is_empty());
    let config = Config::default();
    for path in [
        "/v1/search?related=xyz".to_owned(),
        format!("/v1/search?related={root}&sort=recent"),
        format!("/v1/search?related={root}&window=91d"),
        "/v1/search?window=7d".to_owned(),
    ] {
        assert_eq!(get_with(&config, &pool, &path).await.0, 400, "{path}");
    }
    let (_, info) = get_with(&config, &pool, "/v1/info").await;
    assert_eq!(info["limits"]["related_taggers"], 200);
    assert_eq!(info["limits"]["related_window_days"], 365);
}
