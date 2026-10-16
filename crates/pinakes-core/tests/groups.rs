use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    group_keys, groups,
    model::{Point, ResourceMeta},
    store,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

async fn save(
    pool: &PgPool,
    name: &str,
    keys: &[&str],
    lat: Option<f64>,
) -> (String, ResourceMeta) {
    let uri = format!("https://example.org/{name}");
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    let mut d = ResourceMeta::new(&uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("MusicRecording");
    d.location = lat.map(|lat| Point { lat, lon: 8.0 });
    for key in keys {
        group_keys::add(&mut d, "same_as", key);
    }
    store::save_document(pool, &id, 0, &d).await.unwrap();
    (id, d)
}
async fn gid(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT group_id FROM group_unit WHERE resource_id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn get(app: &axum::Router, path: &str) -> Value {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}
async fn tag(pool: &PgPool, id: &str, author: &str, label: &str) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now())").bind(id).bind(author).bind(label).bind(format!("/pub/app/tags/{id}-{label}")).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::recompute_scores(&mut tx, &[id.into()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn transitive_keys_split_and_curation_preserve_identity(pool: PgPool) {
    let (a, _) = save(&pool, "a", &["https://id.example/one"], Some(47.)).await;
    let (b, _) = save(&pool, "b", &["https://id.example/two"], Some(47.001)).await;
    let (c, mut doc) = save(
        &pool,
        "c",
        &["https://id.example/one", "https://id.example/two"],
        None,
    )
    .await;
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    doc.group_keys.clear();
    store::save_document(&pool, &c, 0, &doc).await.unwrap();
    assert_ne!(gid(&pool, &a).await, gid(&pool, &b).await);
    let (far, _) = save(&pool, "far", &["https://id.example/one"], Some(-47.)).await;
    assert_ne!(gid(&pool, &a).await, gid(&pool, &far).await);
    let id = "a".repeat(32);
    let curate = |ids: Vec<String>| groups::Curation {
        members: ids
            .into_iter()
            .map(|resource_id| groups::Member {
                resource_id,
                occurrence: None,
            })
            .collect(),
        name: Some("Curated".into()),
        image: None,
    };
    let state = AppState::new(pool.clone(), Config::default()).unwrap();
    let make_request = || {
        let mut request = Request::builder()
            .method("PUT")
            .uri(format!("/v1/groups/{id}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&curate(vec![a.clone(), far.clone()])).unwrap(),
            ))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
    };
    assert_eq!(
        api::router(state.clone())
            .oneshot(make_request())
            .await
            .unwrap()
            .status(),
        405
    );
    let response = api::admin_router(state)
        .oneshot(make_request())
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let d: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(d["members"].as_array().unwrap().len(), 2);
    assert_eq!(gid(&pool, &far).await, id);
    groups::curate(&pool, &id, curate(vec![a.clone()]), &Default::default())
        .await
        .unwrap();
    assert_ne!(gid(&pool, &far).await, id);
    assert_eq!(
        store::ensure_resource(&pool, "https://example.org/a")
            .await
            .unwrap(),
        a
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn counts_filters_facets_clusters_and_deleted_best_member(pool: PgPool) {
    let (a, _) = save(&pool, "best", &["https://id.example/one"], None).await;
    let (b, _) = save(&pool, "other", &["https://id.example/one"], Some(47.0001)).await;
    let (c, _) = save(&pool, "separate", &[], Some(47.0002)).await;
    let author = "y".repeat(52);
    let second = "b".repeat(52);
    tag(&pool, &a, &author, "music").await;
    tag(&pool, &b, &author, "music").await;
    tag(&pool, &b, &second, "rare").await;
    tag(&pool, &c, &author, "other").await;
    // A healthy unlocated card must not hide its located sibling from spatial collapse.
    sqlx::query("UPDATE resource SET status='error' WHERE id=$1")
        .bind(&b)
        .execute(&pool)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let card = get(&app, &format!("/v1/resources/{a}")).await;
    assert_eq!(card["group"]["taggers"], 2);
    let group = card["group"]["id"].as_str().unwrap();
    let mut disabled = Config::default();
    disabled.queue.groups.enabled = false;
    let disabled = api::router(AppState::new(pool.clone(), disabled).unwrap());
    assert!(get(&disabled, &format!("/v1/resources/{a}")).await["group"].is_null());
    assert_eq!(get(&disabled, "/v1/info").await["groups"]["enabled"], false);

    assert_eq!(
        get(&app, &format!("/v1/groups/{group}")).await["members"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let search = get(&app, "/v1/search?collapse=group&tags=rare&facet=tags").await;
    assert_eq!(search["items"].as_array().unwrap().len(), 1);
    assert_eq!(search["items"][0]["group"]["id"], group);
    assert!(search["facets"]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v["count"] == 1));
    for sort in ["activity", "recent", "distance"] {
        let result = get(
            &app,
            &format!("/v1/search?collapse=group&near=47,8&radius=1000&sort={sort}"),
        )
        .await;
        assert_eq!(result["items"].as_array().unwrap().len(), 2);
    }
    let cluster = get(
        &app,
        "/v1/search?collapse=group&bbox=7.99,46.99,8.01,47.01&cluster=5&limit=1",
    )
    .await;
    assert_eq!(cluster["clusters"][0]["count"], 2, "{cluster}");
    // Removing the best member keeps the remaining group visible immediately.
    let best: String = sqlx::query_scalar("SELECT best_unit FROM \"group\" WHERE id=$1")
        .bind(group)
        .fetch_one(&pool)
        .await
        .unwrap();
    let remaining = if best == a { &b } else { &a };
    sqlx::query("DELETE FROM resource WHERE id=$1")
        .bind(&best)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&app, "/v1/search?collapse=group").await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let value = get(&app, &format!("/v1/resources/{remaining}")).await;
    assert_eq!(value["group"]["members"].as_array().unwrap().len(), 1);
    sqlx::query("DELETE FROM resource WHERE id=ANY($1)")
        .bind(vec![remaining, c.as_str()])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM \"group\"")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn recurring_uid_is_scoped_to_start_and_page_matches_occurrence(pool: PgPool) {
    use pinakes_core::{config::EventConfig, event::calendar};
    let date = chrono::Utc::now() + chrono::Duration::days(1);
    let feed = "https://calendar.example/test.ics";
    let id = store::ensure_resource(&pool, feed).await.unwrap();
    let text=format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:stable-series\r\nDTSTART:{}\r\nRRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:Series\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",date.format("%Y%m%dT%H%M%SZ"));
    let d = calendar::parse(
        feed,
        text.as_bytes(),
        &EventConfig::default(),
        chrono::Utc::now(),
    )
    .unwrap();
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT group_id) FROM group_unit WHERE resource_id=$1 AND occurrence_key<>''")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    let (page, mut doc) = save(&pool, "page", &[], None).await;
    doc.set_type("Event");
    doc.start = Some(date.with_nanosecond(0).unwrap());
    group_keys::add(&mut doc, "ics_uid", "stable-series");
    store::save_document(&pool, &page, 0, &doc).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(gid(&pool, &page).await)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    // The same URL reused for every date links the matching page only once.
    sqlx::query("UPDATE occurrence SET event_resource_id=$1 WHERE resource_id=$2")
        .bind(&page)
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(DISTINCT group_id) FROM group_unit WHERE resource_id=$1 AND occurrence_key<>''")
        .bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(gid(&pool, &page).await)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(members, 2);
}
use chrono::Timelike;

#[test]
fn typed_group_keys_and_bounded_evidence() {
    let mut d = ResourceMeta::new("https://example.org", "web", 1);
    group_keys::jsonld(
        &mut d,
        &json!({"isrcCode":"US-S1Z-99-00001","sameAs":["https://www.imdb.com/title/tt1234/","https://www.wikidata.org/wiki/Q42"],"identifier":{"propertyID":"ICS UID","value":"uid"}}),
        &"https://example.org".parse().unwrap(),
    );
    assert!(d.group_keys.iter().any(|k| k.key == "isrc:USS1Z9900001"));
    assert!(d.group_keys.iter().any(|k| k.key == "wikidata:Q42"));
    group_keys::add(
        &mut d,
        "musicbrainz",
        "https://musicbrainz.org/recording/28e93528-af3c-40d2-bfdd-6ea5595746f4",
    );
    group_keys::add(
        &mut d,
        "musicbrainz",
        "28E93528-AF3C-40D2-BFDD-6EA5595746F4",
    );
    assert_eq!(
        d.group_keys
            .iter()
            .filter(|k| k.basis == "musicbrainz")
            .count(),
        1
    );
    for _ in 0..50 {
        group_keys::add(&mut d, "wikidata", "Q");
        group_keys::add(&mut d, "imdb", "tt");
        group_keys::add(&mut d, "isrc", "🍉abcdefghi");
    }
    assert!(d.group_keys.len() < 16);
}
