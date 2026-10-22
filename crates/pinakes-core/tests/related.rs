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
/// The defaults, but a row with one shared tagger counts (the ranking tests' rows have few).
fn one_shared() -> Config {
    let mut config = Config::default();
    config.search.related_min_shared = 1;
    config
}
async fn related(pool: &PgPool, id: &str, query: &str) -> Value {
    related_with(&one_shared(), pool, id, query).await
}
async fn related_with(config: &Config, pool: &PgPool, id: &str, query: &str) -> Value {
    let (status, value) = get_with(config, pool, &format!("/v1/search?related={id}{query}")).await;
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
    let app = api::router(AppState::new(pool.clone(), one_shared()).unwrap());
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
    let mut one = one_shared();
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
    let other_alias = store::ensure_resource(&pool, "https://example.org/other-alias")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&other_alias)
        .bind(&other)
        .execute(&pool)
        .await
        .unwrap();
    // A reference stored on an alias counts for its root: tagger 2 tagged the root by its alias
    // and the other resource by its alias, so the other resource has two shared taggers.
    tag(&pool, &root, 1, 1).await;
    tag(&pool, &alias, 2, 1).await;
    tag(&pool, &other, 1, 1).await;
    tag(&pool, &other_alias, 2, 2).await;
    let config = Config::default();
    for seed in [&root, &alias] {
        let v = related_with(&config, &pool, seed, "").await;
        assert_eq!(rows(&v), std::slice::from_ref(&other), "{v}");
    }
    // An unknown resource relates to nothing.
    assert!(rows(&related(&pool, &"0".repeat(32), "").await).is_empty());
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
    assert_eq!(info["limits"]["related_refs_per_tagger"], 1000);
    assert_eq!(info["limits"]["related_min_shared"], 2);
    // The search unit's columns, as the collapsed related page lists them.
    let columns: Vec<String> = sqlx::query_scalar("SELECT column_name::text FROM information_schema.columns WHERE table_name=$1 ORDER BY ordinal_position")
        .bind("search_unit")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(columns.join(","), api::UNIT_COLUMNS);
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_row_needs_two_shared_taggers_its_groups_when_collapsed(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let one = save(&pool, "https://example.org/one", "MusicAlbum").await;
    let two = save(&pool, "https://example.org/two", "MusicAlbum").await;
    let p = save(&pool, "https://example.org/p-tidal", "MusicAlbum").await;
    let q = save(&pool, "https://example.org/p-spotify", "MusicAlbum").await;
    group(&pool, "0123456789abcdef0123456789abcdef", &[&p, &q]).await;
    for who in [1, 2, 3] {
        tag(&pool, &seed, who, 1).await;
    }
    tag(&pool, &one, 1, 1).await;
    tag(&pool, &two, 1, 2).await;
    tag(&pool, &two, 2, 2).await;
    // Each member one shared tagger, the group two.
    tag(&pool, &p, 1, 3).await;
    tag(&pool, &q, 3, 3).await;
    let config = Config::default();
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        std::slice::from_ref(&two)
    );
    let v = related_with(&config, &pool, &seed, "&collapse=group").await;
    let got = rows(&v);
    assert_eq!(got.len(), 2, "{v}");
    assert_eq!(got[0], two);
    assert!(got[1] == p || got[1] == q, "{v}");
    // A resource with one tagger relates to nothing at the default.
    let lone = save(&pool, "https://example.org/lone", "MusicAlbum").await;
    tag(&pool, &lone, 4, 1).await;
    tag(&pool, &one, 4, 1).await;
    tag(&pool, &two, 4, 1).await;
    assert!(rows(&related_with(&config, &pool, &lone, "").await).is_empty());
    assert!(rows(&related_with(&config, &pool, &lone, "&collapse=group").await).is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn each_tagger_counts_only_their_most_recent_references(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let recent = save(&pool, "https://example.org/recent", "MusicAlbum").await;
    let older = save(&pool, "https://example.org/older", "MusicAlbum").await;
    tag(&pool, &seed, 1, 1).await;
    tag(&pool, &recent, 1, 2).await;
    tag(&pool, &older, 1, 3).await;
    let mut config = one_shared();
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        [recent.clone(), older.clone()]
    );
    // Two references read: the seed's tag and the next most recent.
    config.search.related_refs_per_tagger = 2;
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        std::slice::from_ref(&recent)
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_opted_out_member_of_the_seed_s_group_supplies_no_taggers(pool: PgPool) {
    let album = save(&pool, "https://example.org/album", "MusicAlbum").await;
    let post = "https://bsky.app/profile/x/post/1";
    let opted = store::ensure_resource(&pool, post).await.unwrap();
    let mut doc = ResourceMeta::new(post, "bluesky", 86400);
    doc.set_type("SocialMediaPosting");
    doc.attributes.insert("opted_out".into(), "true".into());
    store::save_document(&pool, &opted, 0, &doc).await.unwrap();
    group(&pool, "0123456789abcdef0123456789abcdef", &[&album, &opted]).await;
    let other = save(&pool, "https://example.org/other", "MusicAlbum").await;
    tag(&pool, &album, 1, 1).await;
    tag(&pool, &opted, 2, 1).await;
    tag(&pool, &other, 1, 2).await;
    tag(&pool, &other, 2, 2).await;
    let config = Config::default();
    // Only tagger 1 counts: the other album has one shared tagger, below the floor.
    for seed in [&album, &opted] {
        let v = related_with(&config, &pool, seed, "").await;
        assert!(rows(&v).is_empty(), "{v}");
    }
    // Tagger 2 on the album itself counts.
    tag(&pool, &album, 2, 1).await;
    let v = related_with(&config, &pool, &album, "").await;
    assert_eq!(rows(&v), std::slice::from_ref(&other), "{v}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_group_counts_only_members_that_pass_the_filters_and_never_opted_out_posts(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let album = save(&pool, "https://example.org/album", "MusicAlbum").await;
    let track = save(&pool, "https://example.org/track", "MusicRecording").await;
    group(&pool, "0123456789abcdef0123456789abcdef", &[&album, &track]).await;
    // A post whose author opted out, grouped with another album.
    let other = save(&pool, "https://example.org/other", "MusicAlbum").await;
    let post = "https://bsky.app/profile/x/post/1";
    let opted = store::ensure_resource(&pool, post).await.unwrap();
    let mut doc = ResourceMeta::new(post, "bluesky", 86400);
    doc.set_type("SocialMediaPosting");
    doc.attributes.insert("opted_out".into(), "true".into());
    store::save_document(&pool, &opted, 0, &doc).await.unwrap();
    group(&pool, "fedcba9876543210fedcba9876543210", &[&other, &opted]).await;
    for who in [1, 2] {
        tag(&pool, &seed, who, 1).await;
    }
    tag(&pool, &album, 1, 2).await;
    tag(&pool, &track, 2, 2).await;
    tag(&pool, &other, 1, 2).await;
    tag(&pool, &opted, 2, 2).await;
    let config = Config::default();
    // The album's group has two shared taggers; the other album's only one, as the post's
    // tagger never counts.
    let v = related_with(&config, &pool, &seed, "&collapse=group").await;
    let got = rows(&v);
    assert_eq!(got.len(), 1, "{v}");
    assert!(got[0] == album || got[0] == track, "{v}");
    // Of the albums only, the group has one.
    let v = related_with(&config, &pool, &seed, "&collapse=group&type=MusicAlbum").await;
    assert!(rows(&v).is_empty(), "{v}");
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_taggers_are_the_authors_of_the_seed_s_most_recent_tags(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let old = save(&pool, "https://example.org/old", "MusicAlbum").await;
    let new = save(&pool, "https://example.org/new", "MusicAlbum").await;
    // Taggers 1 and 2 tagged the seed long ago, 3 and 4 lately; tagger 3 twice.
    tag(&pool, &seed, 1, 30).await;
    tag(&pool, &seed, 2, 20).await;
    tag(&pool, &seed, 4, 2).await;
    for days in [1, 3] {
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','rock',$3,now()-$4*interval '1 day')")
            .bind(&seed)
            .bind(author(3))
            .bind(format!("/pub/pubky.app/tags/seed-rock-{days}"))
            .bind(days as f64)
            .execute(&pool)
            .await
            .unwrap();
    }
    for who in [1, 2] {
        tag(&pool, &old, who, 10).await;
    }
    for who in [3, 4] {
        tag(&pool, &new, who, 10).await;
    }
    let mut config = Config::default();
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        [new.clone(), old.clone()]
    );
    // A sample of the three most recent tags, by 3, 4 and 3: two taggers, who relate the seed
    // to `new` only.
    config.search.related_tag_sample = 3;
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        std::slice::from_ref(&new)
    );
    // Three taggers of the whole sample, those with the newest tags: 3, 4 and 2, so `old` has
    // one shared tagger, below the floor.
    config.search.related_tag_sample = 2000;
    config.search.related_taggers = 3;
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        std::slice::from_ref(&new)
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_seed_with_many_replies_and_few_tags_samples_its_taggers(pool: PgPool) {
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let recent = save(&pool, "https://example.org/recent", "MusicAlbum").await;
    let older = save(&pool, "https://example.org/older", "MusicAlbum").await;
    // 50,000 replies to the seed in the window, by as many authors, newer than its tags.
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) SELECT $1,rpad('z'||g,52,'x'),'reply',NULL,'/pub/pubky.app/posts/z'||g,now()-g*interval '1 second' FROM generate_series(1,50000) g")
        .bind(&seed)
        .execute(&pool)
        .await
        .unwrap();
    // Five tags: 1 and 2 the oldest, who also tag `older`; 3, 4 and 5, who also tag `recent`.
    for who in [1, 2, 3, 4, 5] {
        tag(&pool, &seed, who, 10 - who as i64).await;
    }
    for who in [1, 2] {
        tag(&pool, &older, who, 1).await;
    }
    for who in [3, 4, 5] {
        tag(&pool, &recent, who, 1).await;
    }
    let mut config = Config::default();
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        [recent.clone(), older.clone()]
    );
    // The sample is of tags, never of replies: the three most recent tags are 5, 4 and 3's.
    config.search.related_tag_sample = 3;
    assert_eq!(
        rows(&related_with(&config, &pool, &seed, "").await),
        std::slice::from_ref(&recent)
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_collapsed_group_s_row_is_the_member_with_the_most_references_up_to_the_cap(
    pool: PgPool,
) {
    // One group of two albums: `few` has 3 references, `many` 10 (the lower ID is `few`'s or
    // `many`'s, whichever: ties are checked by it).
    let few = save(&pool, "https://example.org/few", "MusicAlbum").await;
    let many = save(&pool, "https://example.org/many", "MusicAlbum").await;
    group(&pool, "0123456789abcdef0123456789abcdef", &[&few, &many]).await;
    for who in 1..=3 {
        tag(&pool, &few, who, 1).await;
    }
    for who in 1..=10 {
        tag(&pool, &many, who, 1).await;
    }
    let mut config = Config::default();
    let row = |config: Config| {
        let pool = pool.clone();
        async move {
            let (status, v) =
                get_with(&config, &pool, "/v1/search?tags=music&collapse=group").await;
            assert_eq!(status, 200, "{v}");
            let got = rows(&v);
            assert_eq!(got.len(), 1, "{v}");
            got[0].clone()
        }
    };
    assert_eq!(row(config.clone()).await, many);
    // Counted up to 5 each: `many` still has more.
    config.search.collapse_refs_per_member = 5;
    assert_eq!(row(config.clone()).await, many);
    // Counted up to 2 each: a tie, broken by the lower ID.
    config.search.collapse_refs_per_member = 2;
    let lower = if few < many { &few } else { &many };
    assert_eq!(row(config.clone()).await, *lower);
    let (_, info) = get_with(&config, &pool, "/v1/info").await;
    assert_eq!(info["limits"]["collapse_refs_per_member"], 2);
    assert_eq!(info["limits"]["related_tag_sample"], 2000);
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_ranking_holds_whatever_the_dates_even_in_year_one(pool: PgPool) {
    // A clock in year 1 and references a few weeks before it: the rows still rank by their
    // shared taggers, then by the newest of their references.
    let seed = save(&pool, "https://example.org/seed", "MusicAlbum").await;
    let three = save(&pool, "https://example.org/three", "MusicAlbum").await;
    let a = save(&pool, "https://example.org/a", "MusicAlbum").await;
    let b = save(&pool, "https://example.org/b", "MusicAlbum").await;
    // Of the two rows with two taggers, the one listed after the other on a tie (the lower ID)
    // has the newer references, so a rank that lost the dates would put them the wrong way.
    let (newer, older) = if a < b { (a, b) } else { (b, a) };
    let at = |id: &str, who: usize, day: &str| {
        let pool = pool.clone();
        let (id, day) = (id.to_owned(), day.to_owned());
        async move {
            sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','music',$3,$4::timestamptz)")
                .bind(&id)
                .bind(author(who))
                .bind(format!("/pub/pubky.app/tags/{id}-{who}-{day}"))
                .bind(format!("0001-{day}T00:00:00Z"))
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    for who in [1, 2, 3] {
        at(&seed, who, "02-20").await;
        at(&three, who, "01-02").await;
    }
    for who in [1, 2] {
        at(&newer, who, "02-10").await;
        at(&older, who, "01-05").await;
    }
    let mut state = AppState::new(pool.clone(), Config::default()).unwrap();
    let now: chrono::DateTime<chrono::Utc> = "0001-03-01T00:00:00Z".parse().unwrap();
    state.clock = Some(std::sync::Arc::new(move || now));
    let app = api::router(state);
    for query in ["", "&window=90d"] {
        let mut request = Request::get(format!("/v1/search?related={seed}{query}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:4000".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            rows(&v),
            [three.clone(), newer.clone(), older.clone()],
            "{query}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn the_seed_links_sampled_are_the_ones_with_the_newest_tags_up_to_the_cap(pool: PgPool) {
    // The seed, two aliases of it and a member of its group, each tagged by its own tagger, the
    // seed most recently; each tagger also tagged a resource of their own.
    let seed = save(&pool, "https://example.org/album", "MusicAlbum").await;
    let first = save(&pool, "https://example.org/album?utm=1", "MusicAlbum").await;
    let second = save(&pool, "https://example.org/album?utm=2", "MusicAlbum").await;
    let member = save(&pool, "https://example.net/album", "MusicAlbum").await;
    for alias in [&first, &second] {
        sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'tracking')")
            .bind(alias)
            .bind(&seed)
            .execute(&pool)
            .await
            .unwrap();
    }
    group(&pool, "0123456789abcdef0123456789abcdef", &[&seed, &member]).await;
    let mut theirs = vec![];
    for (who, link, days) in [
        (1, &seed, 1),
        (2, &first, 2),
        (3, &second, 30),
        (4, &member, 40),
    ] {
        tag(&pool, link, who, days).await;
        let own = save(
            &pool,
            &format!("https://example.org/own-{who}"),
            "MusicAlbum",
        )
        .await;
        tag(&pool, &own, who, 3).await;
        theirs.push(own);
    }
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    for (cap, taggers) in [(1, 1), (2, 2), (3, 3), (4, 4), (32, 4)] {
        let mut config = one_shared();
        config.search.related_seed_links = cap;
        let (_, info) = get_with(&config, &pool, "/v1/info").await;
        assert_eq!(info["limits"]["related_seed_links"], cap, "/v1/info");
        let v = related_with(&config, &pool, &seed, "").await;
        assert_eq!(
            sorted(rows(&v)),
            sorted(theirs[..taggers].to_vec()),
            "related_seed_links={cap}"
        );
    }
}
