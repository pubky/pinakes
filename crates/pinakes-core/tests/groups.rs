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
        if let Some(value) = key.strip_prefix("isrc:") {
            group_keys::add(&mut d, "isrc", value);
        } else {
            group_keys::add(&mut d, "same_as", key);
        }
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
async fn round3_identifier_twins_group_without_redirect_aliases(pool: PgPool) {
    let mut ids = Vec::new();
    // Model legacy cards too: own typed identity must be recovered even without stored keys.
    for uri in [
        "isbn:0140328726",
        "isbn:9780140328721",
        "isbn:9780201616224",
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "isbn", 86400);
        d.set_type("Book");
        d.name = Some("Book".into());
        d.aliases
            .push("https://openlibrary.org/shared-redirect".into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    assert_eq!(gid(&pool, &ids[0]).await, gid(&pool, &ids[1]).await);
    assert_ne!(gid(&pool, &ids[0]).await, gid(&pool, &ids[2]).await);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_group WHERE key='isbn:9780140328721' AND trusted"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    let mut doi_ids = Vec::new();
    for uri in ["doi:10.1000/EXAMPLE", "https://doi.org/10.1000/example"] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, "doi", 86400);
        d.set_type("ScholarlyArticle");
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        doi_ids.push(id);
    }
    assert_eq!(gid(&pool, &doi_ids[0]).await, gid(&pool, &doi_ids[1]).await);
    let mut invalid = ResourceMeta::new("https://example.org/book", "web", 0);
    group_keys::add(&mut invalid, "isbn", "9780140328722");
    group_keys::add(&mut invalid, "doi", "invalid");
    assert!(invalid.group_keys.is_empty());
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
        assert_eq!(pending.await.unwrap(), changed == &other);
        if changed == &b {
            let retry:(i32,f64)=sqlx::query_as("SELECT fence_attempts,extract(epoch FROM(next_attempt_at-now()))::float8 FROM group_dirty WHERE resource_id=$1").bind(&b).fetch_one(&pool).await.unwrap();
            assert_eq!(retry.0, 1);
            assert!(retry.1 > 4.0 && retry.1 <= 10.0);
            for _ in 0..3 {
                sqlx::query("UPDATE resource SET group_revision=group_revision+1 WHERE id=$1")
                    .bind(&b)
                    .execute(&pool)
                    .await
                    .unwrap();
                assert!(!groups::reconcile(&pool, &b, &Default::default()).await);
            }
            assert_eq!(
                sqlx::query_scalar::<_, i32>(
                    "SELECT fence_attempts FROM group_dirty WHERE resource_id=$1"
                )
                .bind(&b)
                .fetch_one(&pool)
                .await
                .unwrap(),
                1
            );
            sqlx::query("UPDATE group_dirty SET next_attempt_at=now() WHERE resource_id=$1")
                .bind(&b)
                .execute(&pool)
                .await
                .unwrap();
            assert!(groups::reconcile(&pool, &b, &Default::default()).await);
        }
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
    disabled.groups.enabled = false;
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
    let text=format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:stable-series\r\nDTSTART:{}\r\nRRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:Series\r\nGEO:47;8\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",date.format("%Y%m%dT%H%M%SZ"));
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
    doc.name = Some("Series".into());
    doc.location = Some(Point {
        lat: 47.0,
        lon: 8.0,
    });
    doc.start = Some(date.with_nanosecond(0).unwrap());
    group_keys::add(&mut doc, "ics_uid", "stable-series");
    store::save_document(&pool, &page, 0, &doc).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(gid(&pool, &page).await)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0); // Default off: a page's copied UID cannot join a feed.
    let cfg = groups::GroupConfig {
        cross_container_occurrences: groups::CrossContainerOccurrences::Corroborated,
        ..Default::default()
    };
    assert!(groups::reconcile(&pool, &page, &cfg).await);
    assert_ne!(gid(&pool, &page).await, page);
    // The same URL reused for every date links the matching page only once.
    sqlx::query("UPDATE occurrence SET event_resource_id=$1 WHERE resource_id=$2")
        .bind(&page)
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    groups::drain(&pool, &cfg, 100).await.unwrap();
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

    // Two tenants can copy the complete public occurrence; default-off prevents joining.
    let cfg = groups::GroupConfig {
        cross_container_occurrences: groups::CrossContainerOccurrences::Corroborated,
        ..Default::default()
    };
    let date = chrono::Utc::now() + chrono::Duration::days(1);
    let mut feeds = Vec::new();
    for (tenant, name, lat) in [
        ("victim", "Community concert", 47.0),
        ("copy", "Community concert copied", 47.001),
    ] {
        let uri = format!("https://calendar.google.com/calendar/ical/{tenant}/public/basic.ics");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let text=format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:public-event\r\nDTSTART:{}\r\nSUMMARY:{name}\r\nGEO:{lat};8\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",date.format("%Y%m%dT%H%M%SZ"));
        let d = pinakes_core::event::calendar::parse(
            &uri,
            text.as_bytes(),
            &Default::default(),
            chrono::Utc::now(),
        )
        .unwrap();
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        sqlx::query("UPDATE occurrence SET doc=doc||jsonb_build_object('image',$2::text) WHERE resource_id=$1").bind(&id).bind(format!("https://images.example/{tenant}.jpg")).execute(&pool).await.unwrap();
        assert!(groups::reconcile(&pool, &id, &Default::default()).await);
        if tenant == "victim" {
            tag(&pool, &id, &"v".repeat(52), "protected").await;
        }
        feeds.push(id);
    }
    let occurrence_group = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_,Option<String>>("SELECT group_id FROM group_unit WHERE resource_id=$1 AND occurrence_key<>'' LIMIT 1").bind(id).fetch_one(&pool).await.unwrap()
        }
    };
    assert!(occurrence_group(feeds[0].clone()).await.is_none());
    assert!(occurrence_group(feeds[1].clone()).await.is_none());
    assert!(groups::reconcile(&pool, &feeds[1], &cfg).await);
    let group = occurrence_group(feeds[0].clone()).await.unwrap();
    assert_eq!(
        occurrence_group(feeds[1].clone()).await.as_deref(),
        Some(group.as_str())
    );
    let card = get(&app, &format!("/v1/groups/{group}")).await;
    assert_eq!(card["name"], "Community concert");
    assert_eq!(card["image"], "https://images.example/victim.jpg");
    assert_eq!(card["location"]["lat"], 47.0);
    let collapsed = get(
        &app,
        "/v1/search?collapse=group&type=Event&tags=protected&fields=document",
    )
    .await;
    let hit = &collapsed["items"][0];
    assert_eq!(hit["name"], card["name"]);
    assert_eq!(hit["image"], card["image"]);
    assert_eq!(
        hit["document"]["location"]["lat"].as_f64(),
        card["location"]["lat"].as_f64()
    );
    // A member occurrence read through search also retains the victim's own facts.
    let member = get(&app, "/v1/search?tags=protected&type=Event&fields=document").await;
    assert_eq!(member["items"][0]["name"], card["name"]);
    assert_eq!(member["items"][0]["image"], card["image"]);
    assert_eq!(
        member["items"][0]["document"]["location"]["lat"].as_f64(),
        card["location"]["lat"].as_f64()
    );
    let records = get(&app, &format!("/v1/resources/{}/occurrences", feeds[0])).await;
    assert_eq!(records["items"][0]["document"]["name"], card["name"]);
    assert_eq!(records["items"][0]["document"]["image"], card["image"]);
    assert_eq!(
        records["items"][0]["document"]["location"]["lat"].as_f64(),
        Some(47.0)
    );
}

#[test]
fn round2_multi_tenant_publishers_and_overrides() {
    let shapes: std::collections::BTreeMap<String, Vec<String>> =
        serde_json::from_str(include_str!("fixtures/tenant-url-shapes.json")).unwrap();
    assert_eq!(shapes.len(), group_keys::TENANT_RULES.len());
    for host in group_keys::TENANT_RULES.keys() {
        let urls = &shapes[host];
        assert_ne!(
            group_keys::origin(&urls[0]),
            group_keys::origin(&urls[2]),
            "{host}"
        );
        assert_eq!(
            group_keys::origin(&urls[0]),
            group_keys::origin(&urls[1]),
            "{host}"
        );
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.runtime");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!("tenants-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"tenant.example":"first_path_segment"}"#).unwrap();
    let mut cfg = groups::GroupConfig {
        multi_tenant_hosts_path: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    cfg.load_tenants().unwrap();
    assert_ne!(
        group_keys::origin_with("https://tenant.example/a/x", &cfg.tenant_rules),
        group_keys::origin_with("https://tenant.example/b/x", &cfg.tenant_rules)
    );
    std::fs::remove_file(path).unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn round2_own_and_legacy_keys_survive_reserved_or_full_stored_evidence(pool: PgPool) {
    let uri = "https://www.wikidata.org/wiki/Q42";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    for i in 100..116 {
        group_keys::add(&mut d, "wikidata", &format!("Q{i}"));
    }
    store::save_document(&pool, &id, 0, &d).await.unwrap();
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM resource_group WHERE resource_id=$1 AND trusted")
            .bind(&id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(keys.contains(&"wikidata:Q42".into()));
    assert!(keys.contains(&format!("url:{id}")));
    d.group_keys.clear();
    for kind in ["curated", "unit", "group", "identity", "automatic"] {
        d.group_keys.push(group_keys::GroupKey {
            key: format!("{kind}:reserved"),
            basis: "same_as".into(),
            confidence: 1.0,
            target: None,
        });
    }
    assert!(d.clone().bounded(&[]).unwrap().group_keys.is_empty());
    store::save_document(&pool, &id, 0, &d).await.unwrap(); // bypasses bounded(), exercises stored evidence filter
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resource_group WHERE resource_id=$1 AND key LIKE '%:reserved'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    for hashed in [false, true] {
        let canonical = format!("https://legacy.example/canonical-{hashed}");
        let (a, _) = claimed(&pool, &canonical, &canonical, false).await;
        let uri = format!("https://legacy.example/alias-{hashed}");
        let b = store::ensure_resource(&pool, &uri).await.unwrap();
        let key = if hashed {
            format!("url:{a}")
        } else {
            format!("url:{canonical}")
        };
        let mut d = ResourceMeta::new(&uri, "web", 3600);
        d.group_keys.push(group_keys::GroupKey {
            key,
            basis: "canonical".into(),
            confidence: 0.8,
            target: None,
        });
        store::save_document(&pool, &b, 0, &d).await.unwrap();
        assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn round3_scaled_pair_budget_handles_config_cap_without_parking(pool: PgPool) {
    let shared = "USAAA2600003";
    sqlx::query("INSERT INTO resource(id,uri,scheme,host) SELECT md5('budget-'||i),'https://budget.example/'||i,'https','budget.example' FROM generate_series(1,999) i").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id) SELECT id,id FROM resource")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_group(unit_id,resource_id,key,basis,confidence,origin,target) SELECT id,id,'isrc:'||$1,'isrc',1.0,'budget.example','budget.example' FROM resource").bind(shared).execute(&pool).await.unwrap();
    let uri = "https://budget.example/new";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.name = Some("Saved despite pair budget".into());
    group_keys::add(&mut d, "isrc", shared);
    let cfg = pinakes_core::config::QueueConfig {
        groups: groups::GroupConfig {
            max_members: 1000,
            max_members_per_origin: 1000,
            ..Default::default()
        },
        ..Default::default()
    };
    pinakes_core::changes::save(&pool, &id, 0, &d, &cfg)
        .await
        .unwrap();
    let state:(String,i32,bool,bool)=sqlx::query_as("SELECT r.name,r.attempts,r.dead_letter,EXISTS(SELECT 1 FROM group_dirty WHERE resource_id=r.id) FROM resource r WHERE r.id=$1").bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(state, ("Saved despite pair budget".into(), 0, false, false));
    let mut tx = pool.begin().await.unwrap();
    groups::sync(&mut tx, &id, &cfg.groups).await.unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn round2_curated_cards_keep_metadata_and_invalid_curation_is_400(pool: PgPool) {
    let (a, _) = save(&pool, "member", &[], None).await;
    let id = "c".repeat(32);
    let body = json!({"members":[{"resource_id":a}],"name":"Curator title","image":"https://images.example/curated.jpg"});
    let state = AppState::new(pool.clone(), Config::default()).unwrap();
    let admin = api::admin_router(state.clone());
    let put = |body: Value| {
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/groups/{id}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    assert_eq!(
        admin
            .clone()
            .oneshot(put(body.clone()))
            .await
            .unwrap()
            .status(),
        200
    );
    let app = api::router(state);
    let read = get(&app, &format!("/v1/resources/{a}")).await;
    assert_eq!(read["group"]["name"], body["name"]);
    assert_eq!(read["group"]["image"], body["image"]);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/resources/lookup?fields=document")
                .extension(axum::extract::ConnectInfo(
                    "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .header("content-type", "application/json")
                .body(Body::from(json!({"ids":[a]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let lookup: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(lookup[0]["group"]["name"], body["name"], "{lookup}");
    for invalid in [
        json!({"members":[]}),
        json!({"members":[{"resource_id":"x"}]}),
        json!({"members":[{"resource_id":"e".repeat(32)}]}),
        json!({"members":[{"resource_id":a}],"name":"x".repeat(513)}),
    ] {
        assert_eq!(
            admin.clone().oneshot(put(invalid)).await.unwrap().status(),
            400
        );
    }
    sqlx::query("DROP FUNCTION refresh_group_best(text[])")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(admin.oneshot(put(body)).await.unwrap().status(), 500);
}
#[sqlx::test(migrations = "../../migrations")]
async fn review_curating_an_automatic_id_pins_exactly_requested_members(pool: PgPool) {
    let (a, _) = save(&pool, "ca", &["isrc:USAAA2600001"], None).await;
    let (b, _) = save(&pool, "cb", &["isrc:USAAA2600001"], None).await;
    let (c, _) = save(&pool, "cc", &["isrc:USAAA2600001"], None).await;
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
    let (a, _) = save(&pool, "stable-a", &["isrc:USAAA2600002"], None).await;
    let (b, mut d) = save(&pool, "stable-b", &["isrc:USAAA2600002"], None).await;
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
        group_keys::add(&mut d, "isrc", "USAAA2600002");
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
    sqlx::query("INSERT INTO resource_score(resource_id,score,updated_at,newest_at) SELECT id,1,now(),now() FROM resource").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO group_score(group_id,score,updated_at,newest_at,taggers,references_count) SELECT id,1,now(),now(),1,2 FROM \"group\"").execute(&pool).await.unwrap();
    sqlx::query("UPDATE resource SET point=ST_SetSRID(ST_MakePoint(8.0,47.0+((substring(uri from '[0-9]+$'))::integer)/100000.0),4326),doc=doc||jsonb_build_object('location',jsonb_build_object('lat',47.0+((substring(uri from '[0-9]+$'))::integer)/100000.0,'lon',8.0))").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();
    sqlx::query("SELECT refresh_group_best(ARRAY(SELECT id FROM \"group\"))")
        .execute(&pool)
        .await
        .unwrap();
    if let Ok(path) = std::env::var("PINAKES_GROUP_FUNCTION_PLAN_FILE") {
        let body: String =
            sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname='group_card'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let group: String = sqlx::query_scalar("SELECT id FROM \"group\" ORDER BY id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let plan: Value = sqlx::query_scalar(&format!(
            "EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON) {}",
            body.replace("g.id=gid", "g.id=$1")
        ))
        .bind(&group)
        .fetch_one(&pool)
        .await
        .unwrap();
        let refresh_body: String =
            sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname='refresh_group_best'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let mut refresh_plans = Vec::new();
        for statement in refresh_body.split(';').filter(|s| !s.trim().is_empty()) {
            let plan: Value = sqlx::query_scalar(&format!(
                "EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON) {}",
                statement.replace("ANY(ids)", "ANY($1)")
            ))
            .bind(vec![&group])
            .fetch_one(&pool)
            .await
            .unwrap();
            refresh_plans.push(plan);
        }
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({"body":body,"group":group,"plan":plan,"refresh_body":refresh_body,"refresh_plans":refresh_plans})).unwrap(),
        )
        .unwrap();
    }
    let mut hydration = Vec::new();
    for groups in [3000, 1500] {
        let mut tx = pool.begin().await.unwrap();
        if groups == 1500 {
            sqlx::query("DELETE FROM \"group\" WHERE id IN (SELECT id FROM \"group\" ORDER BY id OFFSET 1500)").execute(&mut *tx).await.unwrap();
        }
        let plan:Value=sqlx::query_scalar("EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON) SELECT jsonb_agg(group_card(id)) FROM (SELECT id FROM \"group\" ORDER BY id LIMIT 20) page").fetch_one(&mut *tx).await.unwrap();
        let ms = plan[0]["Execution Time"].as_f64().unwrap();
        if ms > 100.0 {
            eprintln!("SOFT group-card hydration target exceeded: {ms} ms");
        }
        assert!(ms < 5000.0);
        hydration.push(json!({"groups":groups,"plan":plan}));
        // Keep the production-size API benchmark intact.
        tx.rollback().await.unwrap();
    }
    if let Ok(path) = std::env::var("PINAKES_GROUP_HYDRATION_PLAN_FILE") {
        std::fs::write(path, serde_json::to_vec_pretty(&hydration).unwrap()).unwrap();
    }
    let mut evidence = Vec::new();
    for request in [
        "/v1/search?collapse=group&bbox=7.9,47.0,8.1,47.006&limit=20",
        "/v1/search?collapse=group&facet=tags&limit=20",
    ] {
        // One connection retains the exact production statement prepared by the API.
        let connection = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let app = api::router(AppState::new(connection.clone(), Config::default()).unwrap());
        let started = std::time::Instant::now();
        let response = get(&app, request).await;
        let api_ms = started.elapsed().as_secs_f64() * 1000.0;
        if api_ms > 100.0 {
            eprintln!(
                "SOFT performance target exceeded: {request}: {api_ms:.3} ms (target 100 ms)"
            );
        }
        assert!(api_ms < 5000.0, "{request}: {api_ms} ms");
        assert_eq!(response["items"].as_array().unwrap().len(), 20);
        if request.contains("facet") {
            assert_eq!(response["facets"]["tags"][0]["count"], count / 2);
        }
        assert!(response["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["group"]["member_count"] == 2 && r["group"].get("members").is_none()));
        let (name,statement,types):(String,String,Vec<String>)=sqlx::query_as("SELECT name,statement,ARRAY(SELECT t::text FROM unnest(parameter_types) t) FROM pg_prepared_statements WHERE statement LIKE 'WITH eligible%' ").fetch_one(&connection).await.unwrap();
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        let mut coords = [7.9, 47.0, 8.1, 47.006].into_iter();
        let params: Vec<String> = types
            .iter()
            .map(|ty| match ty.as_str() {
                "timestamp with time zone" => {
                    format!("'{}'::timestamptz", chrono::Utc::now().to_rfc3339())
                }
                "double precision" => coords.next().unwrap().to_string(),
                "bigint" | "integer" => "21".into(),
                _ => panic!("unexpected search parameter type {ty}"),
            })
            .collect();
        let explain = format!(
            "EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON) EXECUTE {name}({})",
            params.join(",")
        );
        let plan: Value = sqlx::query_scalar(&explain)
            .fetch_one(&connection)
            .await
            .unwrap();
        assert!(
            plan[0]["Execution Time"].as_f64().unwrap() < 5000.0,
            "{request}: {plan}"
        );
        fn scans(value: &Value, out: &mut Vec<Value>) {
            match value {
                Value::Object(map) => {
                    if value["Relation Name"] == "group_unit" {
                        out.push(value.clone());
                    }
                    for v in map.values() {
                        scans(v, out);
                    }
                }
                Value::Array(values) => {
                    for v in values {
                        scans(v, out);
                    }
                }
                _ => {}
            }
        }
        let mut group_scans = Vec::new();
        scans(&plan, &mut group_scans);
        assert!(!group_scans.is_empty());
        assert!(
            group_scans.iter().all(|s| s["Node Type"] != "Seq Scan"),
            "{request}: {}",
            serde_json::to_string_pretty(&group_scans).unwrap()
        );
        assert!(group_scans.iter().any(|s| s["Index Cond"]
            .as_str()
            .is_some_and(|c| c.contains("unit_id"))));
        evidence.push(json!({"request":request,"api_ms":api_ms,"statement":statement,"parameters":params,"explain":plan,"group_unit_scans":group_scans}));
        connection.close().await;
    }
    if let Ok(path) = std::env::var("PINAKES_GROUP_PLAN_FILE") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(
                &json!({"resources":count,"groups":count/2,"queries":evidence}),
            )
            .unwrap(),
        )
        .unwrap();
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn round3_aliases_do_not_group_and_shared_canonicals_demote_every_claimant(pool: PgPool) {
    let mut ids = Vec::new();
    for suffix in ["one", "two"] {
        let uri = format!("https://example.org/{suffix}");
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        let mut doc = ResourceMeta::new(&uri, "web", 86400);
        doc.name = Some(suffix.into());
        doc.aliases.push("https://example.org/landing".into());
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        ids.push((id, doc));
    }
    assert_ne!(gid(&pool, &ids[0].0).await, gid(&pool, &ids[1].0).await);
    for (id, doc) in &mut ids {
        group_keys::add(doc, "canonical", "https://example.org/landing");
        store::save_document(&pool, id, 0, doc).await.unwrap();
    }
    assert_eq!(gid(&pool, &ids[0].0).await, gid(&pool, &ids[1].0).await);
    let (id, mut third) = save(&pool, "three", &["https://example.org/landing"], None).await;
    third.aliases.push("https://example.org/landing".into());
    ids.push((id, third));
    for (id, doc) in &ids {
        store::save_document(&pool, id, 0, doc).await.unwrap();
    }
    assert_ne!(gid(&pool, &ids[0].0).await, gid(&pool, &ids[1].0).await);
    let key = format!(
        "url:{}",
        pinakes_core::model::identity("https://example.org/landing")
            .unwrap()
            .0
    );
    // Round 4: demoted claims stay live (and hidden from planning) so demotion can heal.
    let count: (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER(WHERE NOT demoted),count(*) FILTER(WHERE demoted) FROM resource_group WHERE key=$1",
    )
    .bind(key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, (0, 3));
    for (id, _) in &ids {
        let alias: bool = sqlx::query_scalar(
            "SELECT (doc->'aliases') ? 'https://example.org/landing' FROM resource WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(alias);
    }
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_group_url_demotions_total 1"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn round3_exhausted_pair_budget_commits_partial_plan_and_preserves_established_groups(
    pool: PgPool,
) {
    sqlx::query("INSERT INTO resource(id,uri,scheme,host,point) SELECT md5('partial-'||i),'https://partial.example/'||i,'https','partial.example',ST_SetSRID(ST_MakePoint(8,40+i/100.0),4326) FROM generate_series(1,399) i").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id) SELECT id,id FROM resource")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO resource_group(unit_id,resource_id,key,basis,confidence,origin) SELECT id,id,'isrc:USTST260000'||i,'isrc',1,'partial.example' FROM resource CROSS JOIN generate_series(1,4) i").execute(&pool).await.unwrap();
    let old = "f".repeat(32);
    sqlx::query("INSERT INTO \"group\"(id) VALUES($1)")
        .bind(&old)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE group_unit SET group_id=$1 WHERE resource_id IN(SELECT id FROM resource ORDER BY id LIMIT 2)").bind(&old).execute(&pool).await.unwrap();
    sqlx::query("UPDATE resource_group k SET group_id=u.group_id FROM group_unit u WHERE u.unit_id=k.unit_id").execute(&pool).await.unwrap();
    let uri = "https://partial.example/new";
    let id = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 3600);
    d.name = Some("Saved on partial plan".into());
    d.location = Some(Point {
        lat: 45.0,
        lon: 8.0,
    });
    for i in 1..=4 {
        group_keys::add(&mut d, "isrc", &format!("USTST260000{i}"));
    }
    let cfg = pinakes_core::config::QueueConfig {
        groups: groups::GroupConfig {
            max_members: 400,
            max_members_per_origin: 400,
            ..Default::default()
        },
        ..Default::default()
    };
    pinakes_core::changes::save(&pool, &id, 0, &d, &cfg)
        .await
        .unwrap();
    let marker: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM group_input WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marker.as_deref(), Some("pair_budget"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_unit WHERE group_id=$1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_dirty WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_group_pair_budget_total 1"));
}

#[sqlx::test(migrations = "../../migrations")]
async fn round3_curation_waits_for_writer_and_policy_changes_redirty(pool: PgPool) {
    let (a, _) = save(&pool, "operator", &[], None).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(73491620)")
        .execute(&mut *lock)
        .await
        .unwrap();
    let app = api::admin_router(AppState::new(pool.clone(), Config::default()).unwrap());
    let body = json!({"members":[{"resource_id":a}],"name":"Curated"});
    let put = move || {
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/groups/{}", "a".repeat(32)))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let response = app.oneshot(put()).await.unwrap();
    assert_eq!(response.status(), 503);
    assert!(response.headers().contains_key("retry-after"));
    lock.rollback().await.unwrap();
    assert!(groups::configure(&pool, &Default::default()).await.unwrap());
    sqlx::query("DELETE FROM group_dirty")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!groups::configure(&pool, &Default::default()).await.unwrap());
    let cfg = groups::GroupConfig {
        cross_container_occurrences: groups::CrossContainerOccurrences::Corroborated,
        ..Default::default()
    };
    assert!(groups::configure(&pool, &cfg).await.unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_dirty")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn round3_upgrade_isolates_stale_container_claims_and_requeues_legacy_podcasts(pool: PgPool) {
    let (id, mut doc) = save(&pool, "old-podcast", &[], None).await;
    doc.set_type("PodcastEpisode");
    group_keys::add(&mut doc, "podcast_guid", "episode-one");
    store::save_document(&pool, &id, 0, &doc).await.unwrap();
    sqlx::query("UPDATE resource_group SET origin='example.org' WHERE resource_id=$1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET dead_letter=true,attempts=5,last_error='parse',lease_until=now()+interval '1 hour' WHERE id=$1").bind(&id).execute(&pool).await.unwrap();
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM resource WHERE id=$1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/202610160027_publisher_repair.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let origin: String = sqlx::query_scalar(
        "SELECT origin FROM resource_group WHERE resource_id=$1 AND basis='podcast_guid'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(origin.starts_with("stale:"));
    let state:(bool,i32,bool,i64,f64)=sqlx::query_as("SELECT dead_letter,attempts,lease_until IS NULL,generation,extract(epoch FROM(next_attempt_at-now()))::float8 FROM resource WHERE id=$1").bind(&id).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (state.0, state.1, state.2, state.3),
        (false, 0, true, generation + 1)
    );
    assert!((-1.0..3600.0).contains(&state.4));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_input WHERE resource_id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

fn recorded_json(url: &str, body: &[u8]) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        permanent_redirect: false,
        truncated: false,
        url: url.parse().unwrap(),
        body: body.to_vec(),
        content_type: "application/json".into(),
        status: 200,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    }
}
async fn page(pool: &PgPool, uri: &str, name: &str, claims: &[(&str, &str)]) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("ScholarlyArticle");
    for (basis, value) in claims {
        group_keys::add(&mut d, basis, value);
    }
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn round4_registry_landing_pages_group_publisher_pages_with_identifiers(pool: PgPool) {
    use pinakes_core::resolver::cards;
    let landing = "https://www.nature.com/articles/nature12373";
    let csl = recorded_json(
        "https://api.crossref.org/v1/works/10.1038%2Fnature12373/transform",
        include_bytes!("fixtures/registry/crossref-nature12373.csl.json"),
    );
    // The page carries everything the live candidate showed: a page-supplied DOI, a
    // one-directional sameAs to doi.org and its own canonical.
    let claims = [
        ("doi", "10.1038/nature12373"),
        ("same_as", "https://doi.org/10.1038/nature12373"),
        ("canonical", landing),
    ];
    let mut identifiers = Vec::new();
    for uri in [
        "doi:10.1038/nature12373",
        "https://dx.doi.org/10.1038/nature12373",
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let d = cards::parse_doi(uri, &csl, 86400).unwrap();
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        identifiers.push(id);
    }
    // Before the exact page exists, a same-origin variant whose own canonical is the
    // registry page meets the identifier directly (rule b); that canonical is not trusted.
    let variant = page(
        &pool,
        "https://www.nature.com/articles/nature12373?error=cookies_not_supported",
        "Variant",
        &[("canonical", landing)],
    )
    .await;
    let group = gid(&pool, &variant).await;
    assert_eq!(gid(&pool, &identifiers[0]).await, group);
    let publisher = page(&pool, landing, "Publisher page", &claims).await;
    assert_eq!(gid(&pool, &publisher).await, group);
    for id in &identifiers {
        assert_eq!(gid(&pool, id).await, group);
    }
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT bool_and(trusted) FROM resource_group WHERE basis='registry' AND resource_id=ANY($1)"
    )
    .bind(&identifiers)
    .fetch_one(&pool)
    .await
    .unwrap());

    // A page claiming the DOI and the publisher's canonical from another origin does not:
    // doi.org names nature.com, never the copier.
    let copier = page(&pool, "https://copy.example/nature12373", "Copy", &claims).await;
    assert_ne!(gid(&pool, &copier).await, group);
    // A web document cannot mint registry evidence for itself.
    let forger = store::ensure_resource(&pool, "https://forger.example/a")
        .await
        .unwrap();
    let mut forged = ResourceMeta::new("https://forger.example/a", "web", 86400);
    forged.group_keys.push(group_keys::GroupKey {
        key: format!("url:{}", pinakes_core::model::identity(landing).unwrap().0),
        basis: "registry".into(),
        confidence: 0.9,
        target: Some(landing.into()),
    });
    store::save_document(&pool, &forger, 0, &forged)
        .await
        .unwrap();
    assert_ne!(gid(&pool, &forger).await, group);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_group WHERE resource_id=$1 AND basis='registry'"
        )
        .bind(&forger)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );

    // Rule 4 picks the best member by distinct taggers, not by identifier provenance.
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    tag(&pool, &publisher, &"p".repeat(52), "paper").await;
    tag(&pool, &publisher, &"q".repeat(52), "paper").await;
    tag(&pool, &identifiers[0], &"p".repeat(52), "paper").await;
    let card = get(&app, &format!("/v1/groups/{group}")).await;
    assert_eq!(card["name"], "Publisher page");
    assert_eq!(card["member_count"], 4);
    for author in ["r", "s", "t"] {
        tag(&pool, &identifiers[0], &author.repeat(52), "paper").await;
    }
    let card = get(&app, &format!("/v1/groups/{group}")).await;
    assert_eq!(card["name"], "Nanometre-scale thermometry in a living cell");

    // Open Library: the tagged edition page (no slug) and its ISBN resource group.
    let edition = page(
        &pool,
        "https://openlibrary.org/books/OL7353617M",
        "Edition page",
        &[(
            "canonical",
            "https://openlibrary.org/books/OL7353617M/Fantastic_Mr._Fox",
        )],
    )
    .await;
    let isbn = store::ensure_resource(&pool, "isbn:9780140328721")
        .await
        .unwrap();
    let ol = recorded_json(
        "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
        include_bytes!("fixtures/registry/openlibrary-9780140328721.json"),
    );
    let d = cards::parse_isbn("isbn:9780140328721", "9780140328721", &ol, 86400).unwrap();
    store::save_document(&pool, &isbn, 0, &d).await.unwrap();
    assert_eq!(gid(&pool, &edition).await, gid(&pool, &isbn).await);
    assert_ne!(gid(&pool, &edition).await, group);
}

#[sqlx::test(migrations = "../../migrations")]
async fn round4_upgrade_requeues_identifier_cards_without_registry_pages(pool: PgPool) {
    let mut ids = Vec::new();
    for (uri, source) in [
        ("doi:10.1038/nature12373", "doi"),
        ("isbn:9780140328721", "isbn"),
        ("https://example.org/web", "web"),
    ] {
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        let mut d = ResourceMeta::new(uri, source, 86400);
        d.name = Some("Old card".into());
        store::save_document(&pool, &id, 0, &d).await.unwrap();
        ids.push(id);
    }
    // A card that already records its registry page is not refetched.
    let current = store::ensure_resource(&pool, "doi:10.1000/current")
        .await
        .unwrap();
    let mut d = ResourceMeta::new("doi:10.1000/current", "doi", 86400);
    group_keys::add(&mut d, "registry", "https://publisher.example/current");
    store::save_document(&pool, &current, 0, &d).await.unwrap();
    ids.push(current);
    sqlx::query("UPDATE resource SET lease_until=now()+interval '1 hour',next_attempt_at=now()+interval '30 days' WHERE id=ANY($1)").bind(&ids).execute(&pool).await.unwrap();
    let before: Vec<i64> = sqlx::query_scalar(
        "SELECT generation FROM resource WHERE id=ANY($1) ORDER BY array_position($1,id)",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/202610160028_registry_landing_pages.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let after: Vec<(i64, bool, f64)> = sqlx::query_as("SELECT generation,lease_until IS NULL,extract(epoch FROM(next_attempt_at-now()))::float8 FROM resource WHERE id=ANY($1) ORDER BY array_position($1,id)").bind(&ids).fetch_all(&pool).await.unwrap();
    for (i, (generation, unleased, delay)) in after.iter().enumerate() {
        if i < 2 {
            assert_eq!(*generation, before[i] + 1);
            assert!(*unleased);
            assert!((-1.0..3600.0).contains(delay), "{delay}");
        } else {
            assert_eq!(*generation, before[i]);
            assert!(*delay > 86400.0);
        }
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn round4_squatters_cannot_take_identity_and_demotion_heals_from_live_claims(pool: PgPool) {
    let owner_uri = "https://shared.example/article";
    let twin_uri = "https://twin.example/article";
    let owner = page(&pool, owner_uri, "Owner", &[("same_as", twin_uri)]).await;
    let twin = page(&pool, twin_uri, "Twin", &[("same_as", owner_uri)]).await;
    // Automatic group IDs follow the smallest member unit; compare with the owner's group.
    let group = || gid(&pool, &owner);
    assert_eq!(gid(&pool, &twin).await, group().await);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let owner_key = format!(
        "url:{}",
        pinakes_core::model::identity(owner_uri).unwrap().0
    );
    let row = |id: String| {
        let pool = pool.clone();
        let key = owner_key.clone();
        async move {
            sqlx::query_as::<_, (bool, bool, bool)>(
                "SELECT trusted,counted,demoted FROM resource_group WHERE resource_id=$1 AND key=$2",
            )
            .bind(id)
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let drain = || {
        let pool = pool.clone();
        async move {
            while groups::drain(&pool, &Default::default(), 100)
                .await
                .unwrap()
                > 0
            {}
        }
    };
    // K40: one squatter on the shared origin may join, but never takes the card, however
    // many taggers it farms. The owner is named by the squatter's canonical.
    let squat = &[("canonical", owner_uri)];
    let s1 = page(&pool, "https://shared.example/s1", "Squatter one", squat).await;
    assert_eq!(gid(&pool, &s1).await, group().await);
    for author in ["a", "b", "c"] {
        tag(&pool, &s1, &author.repeat(52), "farm").await;
    }
    drain().await;
    let card = get(&app, &format!("/v1/groups/{}", group().await)).await;
    assert_eq!(card["name"], "Owner");

    // V13: owner plus two squatters demotes the squatters only; the owner keeps its own key
    // and its reciprocal twin.
    let s2 = page(&pool, "https://shared.example/s2", "Squatter two", squat).await;
    drain().await;
    assert_eq!(row(owner.clone()).await, (true, false, false));
    assert_eq!(row(s1.clone()).await, (false, true, true));
    assert_eq!(row(s2.clone()).await, (false, true, true));
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT group_id IS NOT NULL FROM group_unit WHERE resource_id=$1"
    )
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .unwrap());
    assert_eq!(gid(&pool, &twin).await, group().await);
    assert_ne!(gid(&pool, &s1).await, group().await);
    assert_ne!(gid(&pool, &s2).await, group().await);
    // V21: AMP/print variants of the page name it too, but are the same page, not claimants.
    let amp = page(&pool, "https://shared.example/amp/article", "AMP", squat).await;
    let print = page(
        &pool,
        "https://shared.example/article/print",
        "Print",
        squat,
    )
    .await;
    drain().await;
    for variant in [&amp, &print] {
        assert_eq!(row(variant.clone()).await, (false, false, false));
        assert_eq!(gid(&pool, variant).await, group().await);
    }
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_group_url_demotions_total 1"));

    // K40b: dropping one squatter's claim heals the other through the normal drain.
    let mut d = ResourceMeta::new("https://shared.example/s2", "web", 86400);
    d.name = Some("Squatter two, reformed".into());
    store::save_document(&pool, &s2, 0, &d).await.unwrap();
    drain().await;
    assert_eq!(row(s1.clone()).await, (false, true, false));
    assert_eq!(gid(&pool, &s1).await, group().await);
    // A re-demotion counts again, and an orphan deletion (cascade) heals it as well.
    let s3 = page(&pool, "https://shared.example/s3", "Squatter three", squat).await;
    drain().await;
    assert_eq!(row(s1.clone()).await, (false, true, true));
    // Metrics refresh at most every 15 s; a fresh registry reads the durable total.
    let metrics = pinakes_core::telemetry::Metrics::new().unwrap();
    metrics.refresh(&pool).await.unwrap();
    assert!(metrics
        .encode()
        .unwrap()
        .contains("pinakes_group_url_demotions_total 2"));
    sqlx::query("DELETE FROM resource WHERE id=$1")
        .bind(&s3)
        .execute(&pool)
        .await
        .unwrap();
    drain().await;
    assert_eq!(row(s1.clone()).await, (false, true, false));
    assert_eq!(gid(&pool, &s1).await, group().await);
    let card = get(&app, &format!("/v1/groups/{}", group().await)).await;
    assert_eq!(card["name"], "Owner");
}

#[sqlx::test(migrations = "../../migrations")]
async fn round4_lock_contention_retries_shortly_and_partial_plans_are_not_cached(pool: PgPool) {
    let (a, _) = save(&pool, "lock-a", &["https://example.org/lock-shared"], None).await;
    let (b, _) = save(&pool, "lock-b", &["https://example.org/lock-shared"], None).await;
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    // V19: while another writer holds the group lock, retries stay short and uncharged.
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock(73491620)")
        .execute(&mut *holder)
        .await
        .unwrap();
    for _ in 0..6 {
        sqlx::query("INSERT INTO group_dirty(resource_id) VALUES($1) ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=now()")
            .bind(&a).execute(&pool).await.unwrap();
        assert!(!groups::reconcile(&pool, &a, &Default::default()).await);
        let (fence, attempts, error, delay): (i32, i32, String, f64) = sqlx::query_as("SELECT fence_attempts,attempts,last_error,extract(epoch FROM next_attempt_at-now())::float8 FROM group_dirty WHERE resource_id=$1").bind(&a).fetch_one(&pool).await.unwrap();
        assert_eq!((fence, attempts, error.as_str()), (0, 0, "lock_busy"));
        assert!((0.5..=3.1).contains(&delay), "{delay}");
    }
    sqlx::query("SELECT pg_advisory_unlock(73491620)")
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);
    sqlx::query("UPDATE group_dirty SET next_attempt_at=now() WHERE resource_id=$1")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(groups::reconcile(&pool, &a, &Default::default()).await);
    // K44: an input recorded under a partial plan is recomputed, not served from the
    // fingerprint cache, and its marker clears once a full plan succeeds.
    sqlx::query("UPDATE group_input SET last_error='pair_budget' WHERE resource_id=$1")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(groups::reconcile(&pool, &a, &Default::default()).await);
    let marker: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM group_input WHERE resource_id=$1")
            .bind(&a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marker, None);
}
