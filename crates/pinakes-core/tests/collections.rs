//! The public collections that hold a link (`collections`): the feed's `collection` lines kept
//! one per item, listed by `GET /v1/resources/{id}/collections` (the viewer's own first, then
//! newest first, paged), counted on every card (`collections`) and a search filter
//! (`in_collection`).
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::{Config, IngestConfig},
    ingest,
    model::{identity, ResourceMeta},
    nexus::{Change, Changes, FeedPage, Kind, Op},
    store,
    telemetry::Metrics,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

/// Authors: public keys of one letter, no real keys.
fn author(c: char) -> String {
    c.to_string().repeat(52)
}
/// One item of a collection post's version, as staging's Nexus sends it: the post's version
/// file as the object path, the item as the URI.
fn item(op: Op, by: &str, post: &str, version: &str, uri: &str, at: i64) -> Change {
    Change {
        op,
        uri: uri.into(),
        kind: Kind::Collection,
        author: by.into(),
        object_path: format!("/pub/social/v1/posts/{post}/{version}.json"),
        label: None,
        at,
    }
}
/// The collection post's URI, as the route names it.
fn collection(by: &str, post: &str) -> String {
    format!("pubky://{by}/pub/social/v1/posts/{post}")
}
async fn batch(pool: &PgPool, lines: Vec<Change>) {
    let saved = ingest::cursor(pool, "fixture").await.unwrap();
    let next = saved.cursor + lines.len() as i64;
    let page = FeedPage {
        page: Changes {
            lines,
            next_cursor: next,
            skipped: Default::default(),
        },
        epoch: None,
        length: next,
    };
    let applied = ingest::apply(
        pool,
        "fixture",
        &saved,
        &page,
        &IngestConfig::default(),
        &Metrics::new().unwrap(),
    )
    .await
    .unwrap();
    assert!(applied);
}
/// A card with a document, so search and reads show it.
async fn card(pool: &PgPool, uri: &str, name: &str) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86_400);
    d.name = Some(name.into());
    d.set_type("MusicRecording");
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}
fn id(uri: &str) -> String {
    identity(uri).unwrap().0
}
async fn request(app: &axum::Router, path: &str) -> (u16, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
/// The collections listed for a resource, as URIs, in order.
async fn listed(app: &axum::Router, id: &str) -> Vec<String> {
    let (status, page) = request(app, &format!("/v1/resources/{id}/collections")).await;
    assert_eq!(status, 200, "{page}");
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uri"].as_str().unwrap().to_owned())
        .collect()
}
async fn count(app: &axum::Router, id: &str) -> i64 {
    let (status, card) = request(app, &format!("/v1/resources/{id}")).await;
    assert_eq!(status, 200, "{card}");
    card["collections"].as_i64().unwrap()
}

/// Each item of a collection post is kept alone: a removed item drops only itself, put again it
/// is back, an edit (a new version file) keeps the post once, and deleting the post drops all.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_collections_items_are_kept_each_and_dropped_alone(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let a = author('a');
    let links = [
        "https://open.spotify.com/track/1YQWosTIljIvxAgHWTp7KP",
        "https://www.deezer.com/track/1031007",
        "https://snarkypuppy.bandcamp.com/",
    ];
    for (n, link) in links.iter().enumerate() {
        card(&pool, link, &format!("Item {n}")).await;
    }
    let ids: Vec<String> = links.iter().map(|l| id(l)).collect();
    let post = collection(&a, "0035XTZFQH4P2");
    let put = |op, version: &str, link: &str, at| item(op, &a, "0035XTZFQH4P2", version, link, at);
    batch(
        &pool,
        links
            .iter()
            .map(|l| put(Op::Put, "v1", l, 1_793_217_298_040))
            .collect(),
    )
    .await;
    for id in &ids {
        assert_eq!(listed(&app, id).await, vec![post.clone()], "{id}");
        assert_eq!(count(&app, id).await, 1, "{id}");
    }
    let (_, page) = request(&app, &format!("/v1/resources/{}/collections", ids[0])).await;
    assert_eq!(
        page,
        json!({"items": [{"uri": post, "author": a, "indexed_at": "2026-10-28T19:54:58.040Z"}],
               "cursor": null})
    );

    // A removed item drops only itself; put again, it is back.
    batch(&pool, vec![put(Op::Del, "v1", links[1], 1_793_217_300_000)]).await;
    assert_eq!(listed(&app, &ids[1]).await, Vec::<String>::new());
    assert_eq!(count(&app, &ids[1]).await, 0);
    for id in [&ids[0], &ids[2]] {
        assert_eq!(listed(&app, id).await, vec![post.clone()]);
    }
    batch(&pool, vec![put(Op::Put, "v1", links[1], 1_793_217_301_000)]).await;
    assert_eq!(listed(&app, &ids[1]).await, vec![post.clone()]);

    // An edit: the old version's lines go, the new version's come, the post stays one
    // collection, its time the edit's.
    let mut edit: Vec<Change> = links
        .iter()
        .map(|l| put(Op::Del, "v1", l, 1_793_217_400_000))
        .collect();
    edit.extend([links[0], links[2]].map(|l| put(Op::Put, "v2", l, 1_793_217_400_000)));
    batch(&pool, edit).await;
    let (_, page) = request(&app, &format!("/v1/resources/{}/collections", ids[0])).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "{page}");
    assert_eq!(page["items"][0]["uri"], post.as_str());
    assert_eq!(page["items"][0]["indexed_at"], "2026-10-28T19:56:40Z");
    assert_eq!(listed(&app, &ids[1]).await, Vec::<String>::new());
    assert_eq!(listed(&app, &ids[2]).await, vec![post.clone()]);

    // The post deleted: every item goes.
    batch(
        &pool,
        [links[0], links[2]]
            .map(|l| put(Op::Del, "v2", l, 1_793_217_500_000))
            .into(),
    )
    .await;
    for id in &ids {
        assert_eq!(listed(&app, id).await, Vec::<String>::new());
        assert_eq!(count(&app, id).await, 0);
    }
}

/// A post's reply and embeds are kept each too: they share its version file.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_posts_reply_and_embeds_are_kept_each(pool: PgPool) {
    let a = author('a');
    let line = |op, kind, uri: &str| Change {
        op,
        uri: uri.into(),
        kind,
        author: a.clone(),
        object_path: "/pub/social/v1/posts/0035XTZFSDN5E/0035XTZFSDN5E.json".into(),
        label: None,
        at: 1_793_217_298_393,
    };
    batch(
        &pool,
        vec![
            line(Op::Put, Kind::Reply, "https://example.org/thread"),
            line(Op::Put, Kind::Embed, "https://example.org/one"),
            line(Op::Put, Kind::Embed, "https://example.org/two"),
        ],
    )
    .await;
    let kept = || async {
        sqlx::query_scalar::<_, String>("SELECT kind FROM reference ORDER BY kind,resource_id")
            .fetch_all(&pool)
            .await
            .unwrap()
    };
    assert_eq!(kept().await, ["embed", "embed", "reply"]);
    batch(
        &pool,
        vec![line(Op::Del, Kind::Embed, "https://example.org/one")],
    )
    .await;
    assert_eq!(kept().await, ["embed", "reply"]);
    let left: String = sqlx::query_scalar("SELECT resource_id FROM reference WHERE kind='embed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, id("https://example.org/two"));
}

/// A card's collections are those naming its own link or an alias's, each post once; an alias
/// ID reads its root's.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn an_alias_link_counts_for_its_root_once_per_post(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let (a, b) = (author('a'), author('b'));
    let root = card(&pool, "https://example.org/root", "Root").await;
    let alias = store::ensure_resource(&pool, "https://example.org/spelling")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&alias)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    batch(
        &pool,
        vec![
            // One post naming both spellings, and another naming the alias alone.
            item(Op::Put, &a, "P1", "P1", "https://example.org/root", 1_000),
            item(
                Op::Put,
                &a,
                "P1",
                "P1",
                "https://example.org/spelling",
                1_000,
            ),
            item(
                Op::Put,
                &b,
                "P2",
                "P2",
                "https://example.org/spelling",
                2_000,
            ),
        ],
    )
    .await;
    let both = vec![collection(&b, "P2"), collection(&a, "P1")];
    assert_eq!(listed(&app, &root).await, both);
    assert_eq!(listed(&app, &alias).await, both);
    assert_eq!(count(&app, &root).await, 2);
    let (status, cards) = request_lookup(&app, &[&root]).await;
    assert_eq!(status, 200);
    assert_eq!(cards[0]["collections"], 2);
}
async fn request_lookup(app: &axum::Router, ids: &[&str]) -> (u16, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/resources/lookup")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "ids": ids }).to_string()))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// The viewer's own collections first, then newest first, by author and post on a tie; pages
/// continue to the end without repeating, and a cursor holds to its resource and viewer.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_viewers_own_come_first_then_newest_and_pages_continue(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let link = "https://example.org/song";
    let id = card(&pool, link, "Song").await;
    let other = card(&pool, "https://example.org/other", "Other").await;
    let (a, b, c) = (author('a'), author('b'), author('c'));
    batch(
        &pool,
        vec![
            item(Op::Put, &a, "A1", "A1", link, 1_000),
            item(Op::Put, &b, "B1", "B1", link, 2_000),
            item(Op::Put, &c, "C1", "C1", link, 3_000),
            item(Op::Put, &b, "B2", "B2", link, 4_000),
            item(Op::Put, &a, "A2", "A2", link, 3_000),
        ],
    )
    .await;
    let newest = [
        collection(&b, "B2"),
        collection(&a, "A2"),
        collection(&c, "C1"),
        collection(&b, "B1"),
        collection(&a, "A1"),
    ];
    assert_eq!(listed(&app, &id).await, newest);
    let viewed = [
        collection(&a, "A2"),
        collection(&a, "A1"),
        collection(&b, "B2"),
        collection(&c, "C1"),
        collection(&b, "B1"),
    ];
    for (viewer, want) in [(None, &newest), (Some(&a), &viewed)] {
        let query = viewer.map_or(String::new(), |v| format!("&viewer={v}"));
        let mut seen = vec![];
        let mut cursor: Option<String> = None;
        loop {
            let after = cursor
                .as_ref()
                .map_or(String::new(), |c| format!("&cursor={c}"));
            let (status, page) = request(
                &app,
                &format!("/v1/resources/{id}/collections?limit=2{query}{after}"),
            )
            .await;
            assert_eq!(status, 200, "{page}");
            let items = page["items"].as_array().unwrap();
            assert!(items.len() <= 2);
            seen.extend(items.iter().map(|i| i["uri"].as_str().unwrap().to_owned()));
            cursor = page["cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(&seen, want, "{viewer:?}");
    }

    // A cursor is this resource's and this viewer's; bad input is refused.
    let (_, page) = request(
        &app,
        &format!("/v1/resources/{id}/collections?limit=2&viewer={a}"),
    )
    .await;
    let cursor = page["cursor"].as_str().unwrap();
    for (path, status) in [
        (
            format!("/v1/resources/{id}/collections?limit=2&cursor={cursor}"),
            400,
        ),
        (
            format!("/v1/resources/{id}/collections?limit=2&viewer={b}&cursor={cursor}"),
            400,
        ),
        (
            format!("/v1/resources/{other}/collections?limit=2&viewer={a}&cursor={cursor}"),
            400,
        ),
        (
            format!("/v1/resources/{id}/collections?cursor=x{cursor}"),
            400,
        ),
        (format!("/v1/resources/{id}/collections?viewer=nobody"), 400),
        (format!("/v1/resources/{id}/collections?limit=0"), 400),
        (format!("/v1/resources/{id}/collections?limit=101"), 400),
        (format!("/v1/resources/{id}/collections?limit=100"), 200),
        ("/v1/resources/NOT-AN-ID/collections".into(), 400),
        (format!("/v1/resources/{}/collections", "0".repeat(32)), 404),
    ] {
        assert_eq!(request(&app, &path).await.0, status, "{path}");
    }
}

/// `in_collection` keeps the cards a collection holds, an alias's link as its root.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn search_in_collection_keeps_the_cards_it_holds(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let a = author('a');
    let held = card(&pool, "https://example.org/held", "Held").await;
    let root = card(&pool, "https://example.org/root", "Root").await;
    let alias = store::ensure_resource(&pool, "https://example.org/spelling")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&alias)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    let elsewhere = card(&pool, "https://example.org/elsewhere", "Elsewhere").await;
    batch(
        &pool,
        vec![
            item(Op::Put, &a, "P1", "E1", "https://example.org/held", 1_000),
            item(
                Op::Put,
                &a,
                "P1",
                "E1",
                "https://example.org/spelling",
                1_000,
            ),
            item(
                Op::Put,
                &a,
                "P10",
                "P10",
                "https://example.org/elsewhere",
                1_000,
            ),
        ],
    )
    .await;
    let found = |c: &str| {
        let app = app.clone();
        let path = format!("/v1/search?in_collection={c}");
        async move {
            let (status, page) = request(&app, &path).await;
            assert_eq!(status, 200, "{page}");
            let mut ids: Vec<String> = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_owned())
                .collect();
            ids.sort();
            ids
        }
    };
    let mut want = vec![held.clone(), root.clone()];
    want.sort();
    assert_eq!(found(&collection(&a, "P1")).await, want);
    // A post whose ID starts like another's is another collection.
    assert_eq!(found(&collection(&a, "P10")).await, [elsewhere]);
    assert_eq!(
        found(&collection(&author('b'), "P1")).await,
        Vec::<String>::new()
    );
    for bad in [
        "https://example.org/held".to_owned(),
        format!("pubky://{a}/priv/x"),
        "pubky://short/pub/x".to_owned(),
    ] {
        let (status, _) = request(&app, &format!("/v1/search?in_collection={bad}")).await;
        assert_eq!(status, 400, "{bad}");
    }
}

/// Every page of a card's collections, `limit` at a time, as URIs.
async fn all_pages(app: &axum::Router, id: &str, query: &str, limit: usize) -> Vec<String> {
    let mut seen = vec![];
    let mut cursor: Option<String> = None;
    loop {
        let after = cursor
            .as_ref()
            .map_or(String::new(), |c| format!("&cursor={c}"));
        let (status, page) = request(
            app,
            &format!("/v1/resources/{id}/collections?limit={limit}{query}{after}"),
        )
        .await;
        assert_eq!(status, 200, "{page}");
        seen.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["uri"].as_str().unwrap().to_owned()),
        );
        cursor = page["cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return seen;
        }
    }
}

/// A viewer's own collections come first, their 100 newest; any more are listed with the others,
/// by time.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_viewer_has_their_100_newest_first_and_the_rest_by_time(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let link = "https://example.org/song";
    let id = card(&pool, link, "Song").await;
    let (a, b) = (author('a'), author('b'));
    let mut lines: Vec<Change> = (1..=102)
        .map(|k| item(Op::Put, &a, &format!("A{k:03}"), "v", link, k * 1_000))
        .collect();
    lines.push(item(Op::Put, &b, "Bnew", "v", link, 1_000_000));
    lines.push(item(Op::Put, &b, "Bold", "v", link, 500));
    batch(&pool, lines).await;
    let own = |k: i64| collection(&a, &format!("A{k:03}"));
    let mut viewed: Vec<String> = (3..=102).rev().map(own).collect();
    viewed.extend([
        collection(&b, "Bnew"),
        own(2),
        own(1),
        collection(&b, "Bold"),
    ]);
    for limit in [7, 100] {
        assert_eq!(
            all_pages(&app, &id, &format!("&viewer={a}"), limit).await,
            viewed,
            "{limit}"
        );
    }
    let mut newest = vec![collection(&b, "Bnew")];
    newest.extend((1..=102).rev().map(own));
    newest.push(collection(&b, "Bold"));
    assert_eq!(all_pages(&app, &id, "", 9).await, newest);
}

/// A post holding a card's link and its alias, edited (same file, a later instant) with the
/// alias's put skipped: ingest settles the alias's row to the edit's instant, as the rest of
/// Nexus's stage would, so the post is listed once, at its newest, on whichever page that falls.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_post_held_under_two_links_is_listed_once_at_its_newest(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let a = author('a');
    let root = card(&pool, "https://example.org/root", "Root").await;
    let alias = store::ensure_resource(&pool, "https://example.org/spelling")
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_alias(alias_id,root_id,basis) VALUES($1,$2,'canonical')")
        .bind(&alias)
        .bind(&root)
        .execute(&pool)
        .await
        .unwrap();
    let mut lines: Vec<Change> = (1..=6)
        .map(|k| {
            item(
                Op::Put,
                &a,
                &format!("Q{k}"),
                "v",
                "https://example.org/root",
                k * 1_000,
            )
        })
        .collect();
    lines.push(item(
        Op::Put,
        &a,
        "P",
        "v",
        "https://example.org/root",
        1_500,
    ));
    lines.push(item(
        Op::Put,
        &a,
        "P",
        "v",
        "https://example.org/spelling",
        1_500,
    ));
    batch(&pool, lines).await;
    // The edit: its put of the alias item was skipped.
    batch(
        &pool,
        vec![item(
            Op::Put,
            &a,
            "P",
            "v",
            "https://example.org/root",
            3_500,
        )],
    )
    .await;
    let instants: Vec<i64> = sqlx::query_scalar(
        "SELECT (extract(epoch FROM created_at)*1000)::bigint FROM reference WHERE object_path LIKE '%/P/%' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(instants, [3_500, 3_500]);
    let q = |k: u32| collection(&a, &format!("Q{k}"));
    let want = vec![q(6), q(5), q(4), collection(&a, "P"), q(3), q(2), q(1)];
    for limit in [1, 2, 20] {
        assert_eq!(all_pages(&app, &root, "", limit).await, want, "{limit}");
        assert_eq!(
            all_pages(&app, &root, &format!("&viewer={a}"), limit).await,
            want,
            "{limit}"
        );
    }
    assert_eq!(count(&app, &root).await, 7);
}

/// A post's new version (another version file) replaces its older one even when a del of the
/// older version's lines was skipped: the post is listed once, by its one URI.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_new_version_replaces_an_older_one_whose_del_was_skipped(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let a = author('a');
    let link = "https://example.org/song";
    let id = card(&pool, link, "Song").await;
    batch(
        &pool,
        vec![
            item(Op::Put, &a, "R", "v1", link, 1_000),
            item(Op::Put, &a, "S", "S", link, 2_000),
        ],
    )
    .await;
    // The edit: R's version v2, its del of v1's line skipped.
    batch(&pool, vec![item(Op::Put, &a, "R", "v2", link, 5_000)]).await;
    let files: Vec<String> = sqlx::query_scalar(
        "SELECT object_path FROM reference WHERE kind='collection' ORDER BY object_path",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        files,
        [
            "/pub/social/v1/posts/R/v2.json",
            "/pub/social/v1/posts/S/S.json"
        ]
    );
    for limit in [1, 20] {
        assert_eq!(
            all_pages(&app, &id, "", limit).await,
            [collection(&a, "R"), collection(&a, "S")],
            "{limit}"
        );
    }
    // A late replay of the older version does not bring it back above the newer.
    batch(&pool, vec![item(Op::Put, &a, "R", "v1", link, 1_000)]).await;
    assert_eq!(count(&app, &id).await, 2);
}

/// A collection holds at most 100 items (the specs' cap): a new item past them is refused, a put
/// of one it holds is not.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_collection_holds_at_most_100_items(pool: PgPool) {
    let a = author('a');
    let link = |k: i64| format!("https://example.org/item/{k}");
    batch(
        &pool,
        (0..100)
            .map(|k| item(Op::Put, &a, "C", "C", &link(k), 1_000))
            .collect(),
    )
    .await;
    batch(
        &pool,
        vec![
            item(Op::Put, &a, "C", "C", &link(100), 2_000),
            item(Op::Put, &a, "C", "C", &link(7), 2_000),
        ],
    )
    .await;
    let (items, newest): (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE created_at=to_timestamp(2)) FROM reference WHERE kind='collection'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((items, newest), (100, 100));
    let past: i64 = sqlx::query_scalar("SELECT count(*) FROM reference WHERE resource_id=$1")
        .bind(id(&link(100)))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(past, 0);
}

/// The viewer's pinned set is fixed by the first page: a collection of theirs added while they
/// page moves no post between their own and the others.
#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn the_pinned_set_holds_while_a_viewer_pages(pool: PgPool) {
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    let link = "https://example.org/song";
    let id = card(&pool, link, "Song").await;
    let (a, b) = (author('a'), author('b'));
    let mut lines: Vec<Change> = (1..=102)
        .map(|k| item(Op::Put, &a, &format!("A{k:03}"), "v", link, k * 1_000))
        .collect();
    lines.push(item(Op::Put, &b, "Bnew", "v", link, 1_000_000));
    batch(&pool, lines).await;
    let (status, first) = request(
        &app,
        &format!("/v1/resources/{id}/collections?viewer={a}&limit=50"),
    )
    .await;
    assert_eq!(status, 200, "{first}");
    // A new collection of the viewer's, the newest of all, while they page.
    batch(&pool, vec![item(Op::Put, &a, "A103", "v", link, 2_000_000)]).await;
    let mut seen: Vec<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uri"].as_str().unwrap().to_owned())
        .collect();
    let mut cursor = first["cursor"].as_str().map(str::to_owned);
    while let Some(c) = cursor {
        let (status, page) = request(
            &app,
            &format!("/v1/resources/{id}/collections?viewer={a}&limit=50&cursor={c}"),
        )
        .await;
        assert_eq!(status, 200, "{page}");
        seen.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["uri"].as_str().unwrap().to_owned()),
        );
        cursor = page["cursor"].as_str().map(str::to_owned);
    }
    let own = |k: i64| collection(&a, &format!("A{k:03}"));
    // The first page's 100 stay first; the newcomer was added above where paging had reached.
    let mut want: Vec<String> = (3..=102).rev().map(own).collect();
    want.extend([collection(&b, "Bnew"), own(2), own(1)]);
    assert_eq!(seen, want);
}
