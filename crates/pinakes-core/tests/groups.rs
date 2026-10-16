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
    sqlx::query_scalar("SELECT coalesce(group_id,unit_id) FROM group_unit WHERE resource_id=$1")
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
async fn round2_group_fence_ignores_unrelated_saves_and_retries_changed_inputs(pool: PgPool) {
    let (a, _) = save(&pool, "fence-a", &["https://example.org/shared"], None).await;
    let (b, _) = save(&pool, "fence-b", &["https://example.org/shared"], None).await;
    let (other, _) = save(&pool, "unrelated", &[], None).await;
    for changed in [&other, &b] {
        // Holding the publication lock lets the sync compute evidence, then pauses its fence.
        let mut writer = pool.begin().await.unwrap();
        sqlx::query("SELECT revision FROM search_revision WHERE id=1 FOR UPDATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        sqlx::query("DELETE FROM group_input WHERE resource_id=$1")
            .bind(&b)
            .execute(&mut *writer)
            .await
            .unwrap();
        let pool2 = pool.clone();
        let id = b.clone();
        let pending =
            tokio::spawn(async move { groups::reconcile(&pool2, &id, &Default::default()).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query='SELECT revision FROM search_revision WHERE id=1 FOR UPDATE')")
                    .fetch_one(&pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{name}', '\"Concurrent save\"'),name='Concurrent save' WHERE id=$1")
            .bind(changed).execute(&mut *writer).await.unwrap();
        writer.commit().await.unwrap();
        assert!(pending.await.unwrap());
        assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
        let state: (i32, bool, i32, bool) = sqlx::query_as("SELECT r.attempts,r.dead_letter,coalesce(d.attempts,0),coalesce(d.next_attempt_at>now(),false) FROM resource r LEFT JOIN group_dirty d ON d.resource_id=r.id WHERE r.id=$1")
            .bind(&b).fetch_one(&pool).await.unwrap();
        assert_eq!(state, (0, false, 0, false));
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn transitive_keys_split_and_curation_preserve_identity(pool: PgPool) {
    let (a, _) = save(&pool, "a", &["https://example.org/ids/one"], Some(47.)).await;
    let (b, _) = save(&pool, "b", &["https://example.org/ids/two"], Some(47.001)).await;
    let (c, mut doc) = save(
        &pool,
        "c",
        &["https://example.org/ids/one", "https://example.org/ids/two"],
        None,
    )
    .await;
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    doc.group_keys.clear();
    store::save_document(&pool, &c, 0, &doc).await.unwrap();
    assert_ne!(gid(&pool, &a).await, gid(&pool, &b).await);
    let (far, _) = save(&pool, "far", &["https://example.org/ids/one"], Some(-47.)).await;
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
    let (a, _) = save(&pool, "best", &["https://example.org/ids/one"], None).await;
    let (b, _) = save(
        &pool,
        "other",
        &["https://example.org/ids/one"],
        Some(47.0001),
    )
    .await;
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
    assert!(value["group"].is_null());
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
    let feed = "https://example.org/test.ics";
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
        sqlx::query_scalar("SELECT count(DISTINCT coalesce(group_id,unit_id)) FROM group_unit WHERE resource_id=$1 AND occurrence_key<>''")
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
    let count: i64 = sqlx::query_scalar("SELECT count(DISTINCT coalesce(group_id,unit_id)) FROM group_unit WHERE resource_id=$1 AND occurrence_key<>''")
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

async fn claimed(pool: &PgPool, uri: &str, target: &str, typed: bool) -> (String, ResourceMeta) {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.set_type("MusicRecording");
    d.name = Some(uri.into());
    group_keys::add(&mut d, if typed { "isrc" } else { "same_as" }, target);
    store::save_document(pool, &id, 0, &d).await.unwrap();
    (id, d)
}
#[sqlx::test(migrations = "../../migrations")]
async fn review_directional_aliases_and_typed_self_claims_cannot_poison_groups(pool: PgPool) {
    let victim = "https://open.spotify.com/track/real";
    let (v, _) = claimed(&pool, victim, "USABC2600001", true).await;
    let (a, mut fake) = claimed(&pool, "https://attacker.example/fake", victim, false).await;
    fake.name = Some("Impersonated".into());
    fake.image = Some("https://attacker.example/fake.jpg".into());
    fake.location = Some(Point { lat: 47., lon: 8. });
    fake.start = Some(chrono::Utc::now());
    group_keys::add(&mut fake, "isrc", "USABC2600001");
    store::save_document(&pool, &a, 0, &fake).await.unwrap();
    assert_ne!(gid(&pool, &a).await, gid(&pool, &v).await);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let doc = get(&app, &format!("/v1/resources/{v}")).await;
    assert!(doc["group"].is_null());
    assert_eq!(doc["name"], victim);
    let (left, mut d) = claimed(
        &pool,
        "https://one.example/item",
        "https://two.example/item",
        false,
    )
    .await;
    let (right, _) = claimed(
        &pool,
        "https://two.example/item",
        "https://one.example/item",
        false,
    )
    .await;
    assert_eq!(gid(&pool, &left).await, gid(&pool, &right).await);
    let doc = get(&app, &format!("/v1/resources/{left}")).await;
    assert_eq!(doc["group"]["name"], "https://one.example/item");
    d.group_keys.clear();
    store::save_document(&pool, &left, 0, &d).await.unwrap();
    assert_ne!(gid(&pool, &left).await, gid(&pool, &right).await);
    assert!(group_keys::same_origin(
        "https://a.example.co.uk/x",
        "https://b.example.co.uk/y"
    ));
    assert!(!group_keys::same_origin(
        "https://a.wordpress.com/x",
        "https://b.wordpress.com/y"
    ));
}
#[sqlx::test(migrations = "../../migrations")]
async fn review_curating_an_automatic_id_pins_exactly_requested_members(pool: PgPool) {
    let (a, _) = save(&pool, "ca", &["https://example.org/common"], None).await;
    let (b, _) = save(&pool, "cb", &["https://example.org/common"], None).await;
    let (c, _) = save(&pool, "cc", &["https://example.org/common"], None).await;
    let old = gid(&pool, &a).await;
    let mut members = [a, b, c];
    members.sort();
    groups::curate(
        &pool,
        &old,
        groups::Curation {
            members: vec![groups::Member {
                resource_id: members[2].clone(),
                occurrence: None,
            }],
            name: None,
            image: None,
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(gid(&pool, &members[2]).await, old);
    assert_ne!(gid(&pool, &members[0]).await, old);
    assert_eq!(gid(&pool, &members[0]).await, gid(&pool, &members[1]).await);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
#[sqlx::test(migrations = "../../migrations")]
async fn review_group_failure_keeps_document_and_drain_advances(pool: PgPool) {
    let (a, _) = save(&pool, "ga", &["https://example.org/shared"], None).await;
    let (b, mut d) = save(&pool, "gb", &["https://example.org/shared"], None).await;
    let mut cfg = pinakes_core::config::QueueConfig::default();
    cfg.groups.max_graph_units = 1;
    d.name = Some("Saved despite grouping budget".into());
    pinakes_core::changes::save(&pool, &b, 0, &d, &cfg)
        .await
        .unwrap();
    let state: (String, i32, bool) =
        sqlx::query_as("SELECT name,attempts,dead_letter FROM resource WHERE id=$1")
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, ("Saved despite grouping budget".into(), 0, false));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT last_error FROM group_dirty WHERE resource_id=$1")
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "group_failed"
    );
    let healthy = store::ensure_resource(&pool, "https://healthy.example/one")
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_dirty(resource_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    groups::drain(&pool, &cfg.groups, 100).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_dirty WHERE resource_id=$1")
            .bind(&healthy)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(groups::drain(&pool, &cfg.groups, 100).await.unwrap(), 0);
    // The separate input budget also parks without rolling back valid resource facts.
    cfg.groups.max_graph_bytes = 1;
    d.name = Some("Input budget retained".into());
    pinakes_core::changes::save(&pool, &b, 0, &d, &cfg)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM resource WHERE id=$1")
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Input budget retained"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_claim_flood_preserves_existing_group_and_unchanged_keys_do_not_rewrite(
    pool: PgPool,
) {
    let (a, _) = save(&pool, "stable-a", &["https://example.org/key"], None).await;
    let (b, mut d) = save(&pool, "stable-b", &["https://example.org/key"], None).await;
    let group = gid(&pool, &a).await;
    let before: Vec<String> = sqlx::query_scalar(
        "SELECT xmin::text FROM resource_group WHERE resource_id=$1 ORDER BY key",
    )
    .bind(&b)
    .fetch_all(&pool)
    .await
    .unwrap();
    d.name = Some("Only the title changed".into());
    store::save_document(&pool, &b, 0, &d).await.unwrap();
    let after: Vec<String> = sqlx::query_scalar(
        "SELECT xmin::text FROM resource_group WHERE resource_id=$1 ORDER BY key",
    )
    .bind(&b)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let cfg = groups::GroupConfig {
        max_members: 3,
        max_members_per_origin: 2,
        ..Default::default()
    };
    for i in 0..8 {
        let uri = format!("https://example.org/flood-{i}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut d = ResourceMeta::new(&uri, "web", 3600);
        d.set_type("MusicRecording");
        group_keys::add(&mut d, "same_as", "https://example.org/key");
        let q = pinakes_core::config::QueueConfig {
            groups: cfg.clone(),
            ..Default::default()
        };
        pinakes_core::changes::save(&pool, &id, 0, &d, &q)
            .await
            .unwrap();
        assert_eq!(gid(&pool, &a).await, group);
        assert_eq!(gid(&pool, &b).await, group);
        assert_ne!(gid(&pool, &id).await, group);
    }
    let cfg = groups::GroupConfig {
        max_graph_bytes: 1000,
        ..Default::default()
    };
    let mut tx = pool.begin().await.unwrap();
    let error = groups::sync(&mut tx, &a, &cfg).await.unwrap_err();
    assert!(error.to_string().contains("evidence budget"), "{error}");
}
#[sqlx::test(migrations = "../../migrations")]
async fn review_event_aliases_allow_small_start_differences(pool: PgPool) {
    let (a, mut d) = save(
        &pool,
        "event-a",
        &["https://example.org/event-canonical"],
        None,
    )
    .await;
    d.set_type("Event");
    d.start = Some(chrono::Utc::now() + chrono::Duration::days(1));
    store::save_document(&pool, &a, 0, &d).await.unwrap();
    let (b, mut other) = save(
        &pool,
        "event-b",
        &["https://example.org/event-canonical"],
        None,
    )
    .await;
    other.set_type("Event");
    other.start = d.start.map(|v| v + chrono::Duration::seconds(3));
    store::save_document(&pool, &b, 0, &other).await.unwrap();
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    let mut d = ResourceMeta::new("https://example.org", "web", 3600);
    group_keys::url_keys(&mut d, "https://www.themoviedb.org/movie/550-fight-club");
    group_keys::url_keys(&mut d, "https://www.themoviedb.org/movie/550");
    assert_eq!(d.group_keys.len(), 1);
    assert_eq!(d.group_keys[0].key, "tmdb:movie/550");
}

#[sqlx::test(migrations = "../../migrations")]
async fn review_populated_collapse_uses_equality_facets_and_builds_only_page_cards(pool: PgPool) {
    let count = 6000_i32;
    sqlx::query("INSERT INTO resource(id,uri,scheme,host,resolver,type,type_path,name,status,doc) SELECT md5('bench-'||i),'https://bench.example/'||i,'https','bench.example','web','WebPage',ARRAY['Thing','CreativeWork','WebPage'],'Bench '||i,'ok',jsonb_build_object('type','WebPage','type_path',jsonb_build_array('Thing','CreativeWork','WebPage'),'name','Bench '||i,'source','web','url','https://bench.example/'||i,'status','ok','site','bench.example','fetched_at',0,'ttl_secs',3600,'attributes','{}'::jsonb) FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO \"group\"(id,best_unit,best_resource_id,best_occurrence_key,member_count,kind) SELECT md5('bench-group-'||i),md5('bench-'||(i*2+1)),md5('bench-'||(i*2+1)),'',2,'WebPage' FROM generate_series(0,$1/2-1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id,group_id) SELECT md5('bench-'||i),md5('bench-'||i),md5('bench-group-'||((i-1)/2)) FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT md5('bench-'||i),repeat('a',52),'tag','/pub/test/tags/'||i,'bench',now() FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();
    // The same collapse/facet/page shape, with a fixed rank to keep EXPLAIN deterministic.
    let plan:Value=sqlx::query_scalar(r#"EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON)
    WITH eligible AS (SELECT r.*,0::float8 AS rank FROM search_unit r WHERE r.host='bench.example' AND EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id AND f.label='bench')),
    filtered AS (SELECT DISTINCT ON(b.unit_id) b.*,e.rank FROM eligible e LEFT JOIN group_unit u ON u.unit_id=e.unit_id JOIN grouped_search_unit b ON b.unit_id=coalesce(u.group_id,e.unit_id) ORDER BY b.unit_id,e.rank DESC,e.unit_id),
    page AS (SELECT * FROM filtered ORDER BY rank DESC,unit_id DESC LIMIT 21)
    SELECT (SELECT jsonb_agg(to_jsonb(p)||jsonb_build_object('group',group_card(p.unit_id))) FROM page p) AS items,
    (SELECT jsonb_agg(to_jsonb(t)) FROM (SELECT f.label,count(DISTINCT r.unit_id) AS count FROM
      (SELECT r.unit_id,u.resource_id FROM filtered r JOIN group_unit u ON u.group_id=r.unit_id
      UNION ALL SELECT r.unit_id,r.id FROM filtered r WHERE NOT EXISTS(SELECT 1 FROM "group" g WHERE g.id=r.unit_id)) r
      JOIN effective_reference f ON f.resource_id=r.resource_id WHERE f.label IS NOT NULL GROUP BY f.label) t) AS facets"#).fetch_one(&pool).await.unwrap();
    if let Ok(path) = std::env::var("PINAKES_GROUP_PLAN_FILE") {
        std::fs::write(path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    }
    assert!(
        plan[0]["Execution Time"].as_f64().unwrap() < 5000.,
        "{plan}"
    );
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let api_started = std::time::Instant::now();
    let response = get(
        &app,
        "/v1/search?host=bench.example&tags=bench&collapse=group&facet=tags&limit=20",
    )
    .await;
    let api_ms = api_started.elapsed().as_secs_f64() * 1000.;
    assert!(api_ms < 5000., "API wall time {api_ms} ms");
    if let Ok(path) = std::env::var("PINAKES_GROUP_PLAN_FILE") {
        let evidence = json!({"resources":count,"groups":count/2,"page_limit":20,"api_ms":api_ms,"explain":plan});
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
    assert_eq!(response["items"].as_array().unwrap().len(), 20);
    assert_eq!(response["facets"]["tags"][0]["count"], count / 2);
    assert!(response["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["group"]["members"].as_array().unwrap().len() == 2));
}
