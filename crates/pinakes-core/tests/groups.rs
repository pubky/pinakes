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
mod common;

/// A page at `https://example.org/{name}` that cites `doi:10.5555/{c}` in its head for each
/// name in `cites`.
async fn save(
    pool: &PgPool,
    name: &str,
    cites: &[&str],
    lat: Option<f64>,
) -> (String, ResourceMeta) {
    let uri = format!("https://example.org/{name}");
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    let mut d = ResourceMeta::new(&uri, "web", 86400);
    d.name = Some(name.into());
    d.set_type("MusicRecording");
    d.location = lat.map(|lat| Point { lat, lon: 8.0 });
    for c in cites {
        group_keys::add(&mut d, "cites_doi", &format!("10.5555/{c}"));
    }
    store::save_document(pool, &id, 0, &d).await.unwrap();
    (id, d)
}
/// The DOI card `doi:10.5555/{name}` whose registry record names each page
/// `https://example.org/{p}` as its landing page.
async fn card(pool: &PgPool, name: &str, pages: &[&str], lat: Option<f64>) -> String {
    let uri = format!("doi:10.5555/{name}");
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    let mut d = ResourceMeta::new(&uri, "doi", 86400);
    d.name = Some(format!("{name} card"));
    d.set_type("ScholarlyArticle");
    d.location = lat.map(|lat| Point { lat, lon: 8.0 });
    group_keys::add(&mut d, "doi", &format!("10.5555/{name}"));
    for p in pages {
        group_keys::add(&mut d, "registry", &format!("https://example.org/{p}"));
    }
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}
/// An identifier card and the landing page it names, which cites it: the evidence a group needs.
async fn pair(pool: &PgPool, name: &str, lat: Option<f64>) -> (String, String) {
    let card = card(pool, name, &[name], lat).await;
    let (page, _) = save(pool, name, &[name], lat).await;
    (card, page)
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
    let value = serde_json::from_slice(&bytes).unwrap();
    common::check_response("GET", path, status.as_u16(), &value);
    value
}
async fn status(app: &axum::Router, path: &str) -> u16 {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.2:1234".parse::<std::net::SocketAddr>().unwrap(),
    ));
    app.clone()
        .oneshot(request)
        .await
        .unwrap()
        .status()
        .as_u16()
}
/// The `{id, uri, site}` a card's group hint names `id` by.
fn named(id: &str, name: &str) -> Value {
    json!({"id": id, "uri": format!("https://example.org/{name}"), "site": "example.org"})
}
fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}
async fn tag(pool: &PgPool, id: &str, author: &str, label: &str) {
    sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag',$3,$4,now())").bind(id).bind(author).bind(label).bind(format!("/pub/app/tags/{id}-{label}")).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::recompute_scores(&mut tx, &[id.into()])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// ISBN-10 and its ISBN-13, and every DOI spelling, are one resource: the spelling is an alias.
#[sqlx::test(migrations = "../../migrations")]
async fn identifier_spellings_are_aliases_of_one_resource(pool: PgPool) {
    let root = |uri: &'static str| {
        let pool = pool.clone();
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            pinakes_core::alias::resolve_id(&pool, &id).await.unwrap()
        }
    };
    let book = pinakes_core::model::identity("isbn:9780140328721")
        .unwrap()
        .0;
    assert_eq!(root("isbn:0140328726").await, book);
    assert_eq!(root("isbn:978-0-14-032872-1").await, book);
    assert_eq!(root("isbn:9780140328721").await, book);
    assert_ne!(root("isbn:9780201616224").await, book);
    let article = pinakes_core::model::identity("doi:10.1000/example")
        .unwrap()
        .0;
    for uri in [
        "doi:10.1000/EXAMPLE",
        "https://doi.org/10.1000/example",
        "https://dx.doi.org/10.1000/Example",
    ] {
        assert_eq!(root(uri).await, article, "{uri}");
    }
    let routed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resource WHERE resolver IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(routed, 3, "one routed resource per book or article");
    let mut invalid = ResourceMeta::new("https://example.org/book", "web", 0);
    group_keys::add(&mut invalid, "isbn", "9780140328722");
    group_keys::add(&mut invalid, "doi", "invalid");
    assert!(invalid.group_keys.is_empty());
}

#[sqlx::test(migrations = "../../migrations")]
async fn group_fence_ignores_unrelated_saves_and_retries_changed_inputs(pool: PgPool) {
    let (a, b) = pair(&pool, "fence", None).await;
    let (other, _) = save(&pool, "unrelated", &[], None).await;
    for changed in [&other, &b] {
        // An exclusive writer (`store::WriteOrder`) lets the sync compute evidence, then pauses
        // its fence.
        let mut writer = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(73822917)")
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
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query='SELECT pg_advisory_xact_lock_shared(73822917)')")
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
async fn evidence_loss_splits_and_curation_preserves_identity(pool: PgPool) {
    let a = card(&pool, "one", &["near", "far"], Some(47.)).await;
    let (b, mut doc) = save(&pool, "near", &["one"], Some(47.001)).await;
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    // A located member beyond the distance guard never joins automatically.
    let (far, _) = save(&pool, "far", &["one"], Some(-47.)).await;
    assert_ne!(gid(&pool, &a).await, gid(&pool, &far).await);
    // The page stops citing the identifier: the group splits.
    doc.group_keys.clear();
    store::save_document(&pool, &b, 0, &doc).await.unwrap();
    assert_ne!(gid(&pool, &a).await, gid(&pool, &b).await);
    let id = "a".repeat(32);
    let curate = |ids: Vec<String>| groups::Curation {
        members: ids
            .into_iter()
            .map(|resource_id| groups::Member {
                resource_id,
                occurrence: None,
            })
            .collect(),
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
        store::ensure_resource(&pool, "doi:10.5555/one")
            .await
            .unwrap(),
        a
    );
}

/// A group is a hint: every member keeps its own card, references, ranking and counts; each card
/// names the others; `collapse=group` shows the member with the most references, as itself.
#[sqlx::test(migrations = "../../migrations")]
async fn members_keep_their_own_cards_and_counts_and_collapse_is_a_view(pool: PgPool) {
    let a = card(&pool, "joined", &["other"], None).await;
    let (b, _) = save(&pool, "other", &["joined"], Some(47.0001)).await;
    let (c, _) = save(&pool, "separate", &[], Some(47.0002)).await;
    let author = "y".repeat(52);
    let second = "b".repeat(52);
    tag(&pool, &a, &author, "music").await;
    tag(&pool, &b, &author, "music").await;
    tag(&pool, &b, &second, "rare").await;
    tag(&pool, &c, &author, "other").await;
    // An unhealthy member is still a member and still listed.
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
    assert_eq!(card["name"], "joined card", "{card}");
    let group = card["group"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        card["group"],
        json!({"id": group, "size": 2, "members": [named(&b, "other")]})
    );
    let other = get(&app, &format!("/v1/resources/{b}")).await;
    assert_eq!(other["name"], "other", "{other}");
    assert_eq!(
        other["group"]["members"],
        json!([{"id": a, "uri": "doi:10.5555/joined", "site": card["site"]}])
    );
    let mut disabled = Config::default();
    disabled.groups.enabled = false;
    let disabled = api::router(AppState::new(pool.clone(), disabled).unwrap());
    assert!(get(&disabled, &format!("/v1/resources/{a}")).await["group"].is_null());

    // The group read: the joining keys and every member's own card, no merged card.
    let read = get(&app, &format!("/v1/groups/{group}")).await;
    assert!(read.get("best").is_none(), "{read}");
    assert_eq!(read["curated"], false);
    assert!(!read["keys"].as_array().unwrap().is_empty(), "{read}");
    let names: Vec<&str> = read["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    let mut expected = [(a.as_str(), "joined card"), (b.as_str(), "other")];
    expected.sort();
    assert_eq!(names, expected.iter().map(|e| e.1).collect::<Vec<_>>());

    // Uncollapsed, both members are rows of their own, each naming the other.
    let plain = get(&app, "/v1/search?tags=music&facet=tags,type").await;
    let mut rows = ids(&plain);
    rows.sort();
    let mut both = vec![a.clone(), b.clone()];
    both.sort();
    assert_eq!(rows, both, "{plain}");
    let separate = get(&app, "/v1/search?tags=other").await;
    assert!(separate["items"][0]["group"].is_null(), "{separate}");
    // Collapsed: one row, the member with the most references (b: two), as its own card; the
    // facets still count both members.
    let collapsed = get(&app, "/v1/search?collapse=group&tags=music&facet=tags,type").await;
    assert_eq!(ids(&collapsed), vec![b.clone()], "{collapsed}");
    assert_eq!(collapsed["items"][0]["name"], "other");
    assert_eq!(collapsed["facets"], plain["facets"]);
    let music = collapsed["facets"]["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["label"] == "music")
        .unwrap()
        .clone();
    assert_eq!(music["count"], 2, "{collapsed}");
    // Equal references: the lower ID.
    tag(&pool, &a, &second, "rare").await;
    let collapsed = get(&app, "/v1/search?collapse=group&tags=music").await;
    assert_eq!(
        ids(&collapsed),
        vec![a.clone().min(b.clone())],
        "{collapsed}"
    );
    for sort in ["activity", "recent", "distance"] {
        let result = get(
            &app,
            &format!("/v1/search?collapse=group&near=47,8&radius=1000&sort={sort}"),
        )
        .await;
        assert_eq!(ids(&result).len(), 2, "{result}");
    }
    // Clusters count every located link; collapse is a list view only.
    let cluster = get(
        &app,
        "/v1/search?bbox=7.99,46.99,8.01,47.01&cluster=5&limit=1",
    )
    .await;
    assert_eq!(cluster["clusters"][0]["count"], 2, "{cluster}");
    assert_eq!(
        status(
            &app,
            "/v1/search?collapse=group&bbox=7.99,46.99,8.01,47.01&cluster=5"
        )
        .await,
        400
    );
    // Removing a member leaves the other its own row; after the next run it has no group.
    sqlx::query("DELETE FROM resource WHERE id=$1")
        .bind(&a)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(ids(&get(&app, "/v1/search?collapse=group").await).len(), 2);
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let value = get(&app, &format!("/v1/resources/{b}")).await;
    assert!(value["group"].is_null());
    sqlx::query("DELETE FROM resource WHERE id=ANY($1)")
        .bind(vec![b.as_str(), c.as_str()])
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
async fn recurring_uid_is_scoped_to_start_and_a_page_joins_only_when_corroborated(pool: PgPool) {
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
    // The occurrence route names the joined instance's group, as every card does.
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let joined = json!(gid(&pool, &page).await);
    let items = get(&app, &format!("/v1/resources/{id}/occurrences")).await["items"].clone();
    let groups: Vec<&Value> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["group"])
        .collect();
    assert_eq!(
        groups.iter().filter(|g| g["id"] == joined).count(),
        1,
        "{items}"
    );
    assert_eq!(groups.iter().filter(|g| g.is_null()).count(), 1, "{items}");
}
/// A UID is free text: a NIP-52 coordinate, a URN or `{feed}:{uid}` never spells another
/// feed's key, and no UID joins across feeds unless the corroborated mode is on.
#[sqlx::test(migrations = "../../migrations")]
async fn forged_or_colon_uids_never_join_across_feeds(pool: PgPool) {
    use pinakes_core::{config::EventConfig, event::calendar};
    let date = (chrono::Utc::now() + chrono::Duration::days(1)).format("%Y%m%dT%H%M%SZ");
    let save_feed = |uri: &'static str, uids: Vec<String>| {
        let pool = pool.clone();
        let date = date.to_string();
        async move {
            let id = store::ensure_resource(&pool, uri).await.unwrap();
            let events: String = uids.iter().map(|uid| format!("BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:{date}\r\nSUMMARY:Same night\r\nGEO:47;8\r\nEND:VEVENT\r\n")).collect();
            let text = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{events}END:VCALENDAR\r\n");
            let d = calendar::parse(
                uri,
                text.as_bytes(),
                &EventConfig::default(),
                chrono::Utc::now(),
            )
            .unwrap();
            store::save_document(&pool, &id, 0, &d).await.unwrap();
            id
        }
    };
    let victim = save_feed("https://victim.example/a.ics", vec!["victim".into()]).await;
    let coordinate = format!("31923:{}:party", "a".repeat(64));
    let forger = save_feed(
        "https://forger.example/b.ics",
        vec![format!("{victim}:victim"), "a:b".into(), coordinate.clone()],
    )
    .await;
    let third = save_feed(
        "https://third.example/c.ics",
        vec!["a:b".into(), coordinate],
    )
    .await;
    let grouped = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM group_unit WHERE group_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(
        grouped(pool.clone()).await,
        0,
        "the default mode never joins feeds"
    );
    let cfg = groups::GroupConfig {
        cross_container_occurrences: groups::CrossContainerOccurrences::Corroborated,
        ..Default::default()
    };
    for id in [&victim, &forger, &third] {
        groups::reconcile(&pool, id, &cfg).await;
    }
    groups::drain(&pool, &cfg, 100).await.unwrap();
    // Corroborated: the two equal UIDs join pairwise; the forged one never reaches the victim.
    let groups: Vec<(i64, Vec<String>)> = sqlx::query_as(
        "SELECT count(*),array_agg(DISTINCT resource_id ORDER BY resource_id) FROM group_unit WHERE group_id IS NOT NULL GROUP BY group_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut pair = vec![forger.clone(), third.clone()];
    pair.sort();
    assert_eq!(groups, vec![(2, pair.clone()), (2, pair)]);
}
use chrono::Timelike;

#[test]
fn tenant_rules_scope_publishers() {
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
}

#[sqlx::test(migrations = "../../migrations")]
async fn curation_sets_members_only_and_invalid_curation_is_refused(pool: PgPool) {
    let (a, _) = save(&pool, "member", &[], None).await;
    let id = "c".repeat(32);
    let body = json!({"members":[{"resource_id":a}]});
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
    let response = admin.clone().oneshot(put(body.clone())).await.unwrap();
    assert_eq!(response.status(), 200);
    let curated: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    common::check_response("GET", &format!("/v1/groups/{id}"), 200, &curated);
    assert_eq!(curated["curated"], true);
    assert_eq!(curated["members"][0]["id"], a.as_str());
    let app = api::router(state);
    tag(&pool, &a, &"t".repeat(52), "curated").await;
    assert_eq!(get(&app, &format!("/v1/groups/{id}")).await, curated);
    let collapsed = get(&app, "/v1/search?collapse=group&tags=curated").await;
    assert_eq!(collapsed["items"][0]["name"], "member", "{collapsed}");
    for invalid in [
        json!({"members":[]}),
        json!({"members":[{"resource_id":"x"}]}),
        json!({"members":[{"resource_id":"e".repeat(32)}]}),
    ] {
        assert_eq!(
            admin.clone().oneshot(put(invalid)).await.unwrap().status(),
            400
        );
    }
    // A group has no name or image of its own.
    let named = json!({"members":[{"resource_id":a}],"name":"Curator title"});
    assert_eq!(
        admin.clone().oneshot(put(named)).await.unwrap().status(),
        422
    );
    sqlx::query("ALTER TABLE group_override RENAME TO group_override_gone")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(admin.oneshot(put(body)).await.unwrap().status(), 500);
}
#[sqlx::test(migrations = "../../migrations")]
async fn curating_an_automatic_id_pins_exactly_requested_members(pool: PgPool) {
    let a = card(&pool, "ca", &["cb", "cc"], None).await;
    let (b, _) = save(&pool, "cb", &["ca"], None).await;
    let (c, _) = save(&pool, "cc", &["ca"], None).await;
    let old = gid(&pool, &a).await;
    assert_eq!(gid(&pool, &c).await, old);
    groups::curate(
        &pool,
        &old,
        groups::Curation {
            members: vec![groups::Member {
                resource_id: c.clone(),
                occurrence: None,
            }],
        },
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(gid(&pool, &c).await, old);
    assert_ne!(gid(&pool, &a).await, old);
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
#[sqlx::test(migrations = "../../migrations")]
async fn group_failure_keeps_document_and_drain_advances(pool: PgPool) {
    let a = card(&pool, "ga", &["gb"], None).await;
    let (b, mut d) = save(&pool, "gb", &["ga"], None).await;
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

/// A hint names the group's size and eight other members, the first by ID, whichever member asks.
#[sqlx::test(migrations = "../../migrations")]
async fn a_hint_names_eight_other_members_by_id(pool: PgPool) {
    sqlx::query("INSERT INTO resource(id,uri,host,resolver,type,type_path,name,status,doc) SELECT md5('hint-'||i),'https://hint.example/'||i,'hint.example','web','WebPage',ARRAY['Thing','CreativeWork','WebPage'],'Hint '||i,'ok',jsonb_build_object('type','WebPage','name','Hint '||i,'source','web','url','https://hint.example/'||i,'status','ok','site','hint.example','fetched_at',0,'ttl_secs',3600,'attributes','{}'::jsonb) FROM generate_series(1,12) i").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO \"group\"(id) VALUES(md5('hint-group'))")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id,group_id) SELECT md5('hint-'||i),md5('hint-'||i),md5('hint-group') FROM generate_series(1,12) i").execute(&pool).await.unwrap();
    let mut ids: Vec<String> =
        sqlx::query_scalar("SELECT md5('hint-'||i) FROM generate_series(1,12) i")
            .fetch_all(&pool)
            .await
            .unwrap();
    ids.sort();
    for own in [&ids[0], &ids[4], &ids[11]] {
        let hint: Value = sqlx::query_scalar("SELECT group_hint($1)")
            .bind(own)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(hint["size"], 12);
        let members: Vec<&str> = hint["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = ids
            .iter()
            .filter(|id| *id != own)
            .take(8)
            .map(String::as_str)
            .collect();
        assert_eq!(members, expected, "{own}");
    }
}

/// A resource with many units in one group (a feed's occurrences) never crowds the others out:
/// the hint lists up to eight other resources, one entry each, before its own feed's siblings.
#[sqlx::test(migrations = "../../migrations")]
async fn a_hint_lists_other_resources_before_a_feeds_many_occurrences(pool: PgPool) {
    sqlx::query("INSERT INTO resource(id,uri,host,resolver,type,type_path,name,status,doc) SELECT md5('multi-'||i),'https://multi.example/'||i,'multi.example','web','WebPage',ARRAY['Thing','CreativeWork','WebPage'],'Multi '||i,'ok',jsonb_build_object('type','WebPage','name','Multi '||i,'source','web','url','https://multi.example/'||i,'status','ok','site','multi.example','fetched_at',0,'ttl_secs',3600,'attributes','{}'::jsonb) FROM generate_series(0,10) i").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO \"group\"(id) VALUES(md5('multi-group'))")
        .execute(&pool)
        .await
        .unwrap();
    // The resource with the lowest ID holds twenty units; ten others hold one each.
    let mut ids: Vec<String> =
        sqlx::query_scalar("SELECT md5('multi-'||i) FROM generate_series(0,10) i")
            .fetch_all(&pool)
            .await
            .unwrap();
    ids.sort();
    let feed = ids[0].clone();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id,occurrence_key,group_id) SELECT md5('unit-'||n),$1,'occurrence-'||n,md5('multi-group') FROM generate_series(1,20) n")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id,group_id) SELECT id,id,md5('multi-group') FROM unnest($1::text[]) id")
        .bind(&ids[1..])
        .execute(&pool)
        .await
        .unwrap();
    let hint = |unit: String| {
        let pool = pool.clone();
        async move {
            let v: Value = sqlx::query_scalar("SELECT group_hint($1)")
                .bind(unit)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(v["size"], 30);
            v["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    // Asked from a one-unit resource: the feed once, then seven other resources, by ID.
    let mut expected: Vec<String> = ids
        .iter()
        .filter(|i| **i != ids[5])
        .take(8)
        .cloned()
        .collect();
    expected.sort();
    assert_eq!(hint(ids[5].clone()).await, expected);
    // Asked from one of the feed's occurrences: eight other resources, its siblings after them.
    let unit: String = sqlx::query_scalar("SELECT md5('unit-1')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hint(unit).await, ids[1..9].to_vec());
}

#[sqlx::test(migrations = "../../migrations")]
async fn populated_collapse_and_hints_build_only_page_cards(pool: PgPool) {
    let count = 6000_i32;
    sqlx::query("INSERT INTO resource(id,uri,host,resolver,type,type_path,name,status,doc) SELECT md5('bench-'||i),'https://bench.example/'||i,'bench.example','web','WebPage',ARRAY['Thing','CreativeWork','WebPage'],'Bench '||i,'ok',jsonb_build_object('type','WebPage','type_path',jsonb_build_array('Thing','CreativeWork','WebPage'),'name','Bench '||i,'source','web','url','https://bench.example/'||i,'status','ok','site','bench.example','fetched_at',0,'ttl_secs',3600,'attributes','{}'::jsonb) FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO \"group\"(id) SELECT md5('bench-group-'||i) FROM generate_series(0,$1/2-1) i",
    )
    .bind(count)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO group_unit(unit_id,resource_id,group_id) SELECT md5('bench-'||i),md5('bench-'||i),md5('bench-group-'||((i-1)/2)) FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO reference(resource_id,author,kind,object_path,label,created_at) SELECT md5('bench-'||i),repeat('a',52),'tag','/pub/test/tags/'||i,'bench',now() FROM generate_series(1,$1) i").bind(count).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO resource_score(resource_id,score,updated_at,newest_at) SELECT id,1,now(),now() FROM resource").execute(&pool).await.unwrap();
    sqlx::query("UPDATE resource SET point=ST_SetSRID(ST_MakePoint(8.0,47.0+((substring(uri from '[0-9]+$'))::integer)/100000.0),4326),doc=doc||jsonb_build_object('location',jsonb_build_object('lat',47.0+((substring(uri from '[0-9]+$'))::integer)/100000.0,'lon',8.0))").execute(&pool).await.unwrap();
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();
    // One page of hints, read through the function every card uses.
    let plan:Value=sqlx::query_scalar("EXPLAIN(ANALYZE,BUFFERS,VERBOSE,FORMAT JSON) SELECT jsonb_agg(group_hint(unit_id)) FROM (SELECT unit_id FROM group_unit ORDER BY unit_id LIMIT 20) page").fetch_one(&pool).await.unwrap();
    let ms = plan[0]["Execution Time"].as_f64().unwrap();
    if ms > 100.0 {
        eprintln!("SOFT group-hint hydration target exceeded: {ms} ms");
    }
    assert!(ms < 5000.0);
    if let Ok(path) = std::env::var("PINAKES_GROUP_HYDRATION_PLAN_FILE") {
        std::fs::write(path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    }
    let mut evidence = Vec::new();
    for request in [
        "/v1/search?collapse=group&bbox=7.9,47.0,8.1,47.006&limit=20",
        "/v1/search?collapse=group&facet=tags&limit=20",
        // Every row names its group; the hints are read for the page only.
        "/v1/search?bbox=7.9,47.0,8.1,47.006&limit=20",
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
        // About 14 times the measured time on a loaded machine: a per-row scan fails here.
        assert!(api_ms < 1000.0, "{request}: {api_ms} ms");
        assert_eq!(response["items"].as_array().unwrap().len(), 20);
        if request.contains("facet") {
            // Facets count links, collapsed or not.
            assert_eq!(response["facets"]["tags"][0]["count"], count);
        }
        assert!(response["items"].as_array().unwrap().iter().all(
            |r| r["group"]["size"] == 2 && r["group"]["members"].as_array().unwrap().len() == 1
        ));
        let (name,statement,types):(String,String,Vec<String>)=sqlx::query_as("SELECT name,statement,ARRAY(SELECT t::text FROM unnest(parameter_types) t) FROM pg_prepared_statements WHERE statement LIKE 'WITH eligible%' OR statement LIKE 'WITH filtered%'").fetch_one(&connection).await.unwrap();
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
            plan[0]["Execution Time"].as_f64().unwrap() < 1000.0,
            "{request}: {plan}"
        );
        evidence.push(json!({"request":request,"api_ms":api_ms,"statement":statement,"parameters":params,"explain":plan}));
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
async fn curation_waits_for_writer_and_policy_changes_redirty(pool: PgPool) {
    let (a, _) = save(&pool, "operator", &[], None).await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(73491620)")
        .execute(&mut *lock)
        .await
        .unwrap();
    let app = api::admin_router(AppState::new(pool.clone(), Config::default()).unwrap());
    let body = json!({"members":[{"resource_id":a}]});
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
async fn registry_landing_pages_group_publisher_pages_with_identifiers(pool: PgPool) {
    use pinakes_core::resolver::cards;
    let landing = "https://www.nature.com/articles/nature12373";
    let csl = recorded_json(
        "https://api.crossref.org/v1/works/10.1038%2Fnature12373/transform",
        include_bytes!("fixtures/registry/crossref-nature12373.csl.json"),
    );
    let claims = [("cites_doi", "10.1038/nature12373")];
    let uri = "doi:10.1038/nature12373";
    let identifier = store::ensure_resource(&pool, uri).await.unwrap();
    let d = cards::parse_doi(uri, &csl, 86400).unwrap();
    store::save_document(&pool, &identifier, 0, &d)
        .await
        .unwrap();
    // The landing page the registry names, citing the DOI in its head, joins the card.
    let publisher = page(&pool, landing, "Publisher page", &claims).await;
    let group = gid(&pool, &publisher).await;
    assert_eq!(gid(&pool, &identifier).await, group);
    assert_ne!(group, publisher);
    // A page citing the DOI that the registry does not name stays apart, on any origin.
    for uri in [
        "https://copy.example/nature12373",
        "https://www.nature.com/articles/nature12373?error=cookies_not_supported",
    ] {
        let copier = page(&pool, uri, "Copy", &claims).await;
        assert_ne!(gid(&pool, &copier).await, group, "{uri}");
    }
    // A web document cannot mint registry evidence for itself.
    let forger = store::ensure_resource(&pool, "https://forger.example/a")
        .await
        .unwrap();
    let mut forged = ResourceMeta::new("https://forger.example/a", "web", 86400);
    group_keys::add(&mut forged, "registry", landing);
    group_keys::add(&mut forged, "cites_doi", "10.1038/nature12373");
    store::save_document(&pool, &forger, 0, &forged)
        .await
        .unwrap();
    assert_ne!(gid(&pool, &forger).await, group);
    // The group lists both members' own cards and the citation that joined them.
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let read = get(&app, &format!("/v1/groups/{group}")).await;
    let mut members: Vec<&str> = read["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    members.sort();
    let mut expected = vec![identifier.as_str(), publisher.as_str()];
    expected.sort();
    assert_eq!(members, expected, "{read}");
    assert!(
        read["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k.as_str().unwrap().starts_with("cite:")),
        "{read}"
    );

    // Open Library: the registry's own edition page needs no citation.
    let edition = page(
        &pool,
        "https://openlibrary.org/books/OL7353617M",
        "Edition page",
        &[],
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

fn recorded_html(url: &str, body: &[u8]) -> pinakes_core::resolver::egress::Fetched {
    pinakes_core::resolver::egress::Fetched {
        content_type: "text/html".into(),
        ..recorded_json(url, body)
    }
}
async fn doi_card(pool: &PgPool, uri: &str, csl: &Value) -> String {
    let id = store::ensure_resource(pool, uri).await.unwrap();
    let fetched = recorded_json(
        "https://api.crossref.org/v1/works/x/transform",
        &serde_json::to_vec(csl).unwrap(),
    );
    let d = pinakes_core::resolver::cards::parse_doi(uri, &fetched, 86400).unwrap();
    store::save_document(pool, &id, 0, &d).await.unwrap();
    id
}

#[sqlx::test(migrations = "../../migrations")]
async fn registry_landing_pages_need_the_page_to_cite_the_identifier(pool: PgPool) {
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    // A victim article that cites no DOI, already tagged.
    let victim = page(
        &pool,
        "https://victim.example/article",
        "Victim article",
        &[],
    )
    .await;
    tag(&pool, &victim, &"v".repeat(52), "paper").await;
    // Registrants choose their landing URL: an attacker DOI names the victim's page and
    // farms taggers on its own card.
    let attacker = doi_card(
        &pool,
        "doi:10.5555/evil",
        &json!({"type":"article-journal","title":"Attacker title","DOI":"10.5555/evil",
            "URL":"https://victim.example/article",
            "resource":{"primary":{"URL":"https://victim.example/article"}},
            "link":[{"URL":"https://victim.example/article","content-type":"text/html"}]}),
    )
    .await;
    for author in ["a", "b", "c", "d"] {
        tag(&pool, &attacker, &author.repeat(52), "paper").await;
    }
    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT doc->'group_keys' @> '[{\"basis\":\"registry\"}]' FROM resource WHERE id=$1"
    )
    .bind(&attacker)
    .fetch_one(&pool)
    .await
    .unwrap());
    assert_ne!(gid(&pool, &attacker).await, gid(&pool, &victim).await);

    // The live publisher page, extracted from the recorded page head: it cites its DOI in its
    // head meta, so the Crossref registration meets it.
    let landing = "https://www.nature.com/articles/nature12373";
    let html = recorded_html(
        landing,
        include_bytes!("fixtures/registry/nature12373-head.html"),
    );
    let mut d = pinakes_core::resolver::web::extract(landing, &html, 86400).doc;
    assert!(d
        .group_keys
        .iter()
        .any(|k| k.key == "doi:10.1038/nature12373" && k.basis == "cites_doi"));
    d.name = Some("Publisher page".into());
    let publisher = store::ensure_resource(&pool, landing).await.unwrap();
    store::save_document(&pool, &publisher, 0, &d)
        .await
        .unwrap();
    let csl: Value = serde_json::from_slice(include_bytes!(
        "fixtures/registry/crossref-nature12373.csl.json"
    ))
    .unwrap();
    let genuine = doi_card(&pool, "doi:10.1038/nature12373", &csl).await;
    let group = gid(&pool, &publisher).await;
    assert_eq!(gid(&pool, &genuine).await, group);
    // A second registration naming the same page is not cited by it and stays apart, even
    // though the genuine card naming that page is already a member.
    let squatter = doi_card(
        &pool,
        "doi:10.5555/squat",
        &json!({"type":"article-journal","title":"Squatter title","DOI":"10.5555/squat",
            "resource":{"primary":{"URL":landing}}}),
    )
    .await;
    assert_ne!(gid(&pool, &squatter).await, group);
    assert_ne!(gid(&pool, &squatter).await, gid(&pool, &attacker).await);
    // Each member keeps its own card whatever the taggers; the identifier card names the page.
    for author in ["e", "f", "g"] {
        tag(&pool, &publisher, &author.repeat(52), "paper").await;
    }
    let own = get(&app, &format!("/v1/resources/{publisher}")).await;
    assert_eq!(own["name"], "Publisher page");
    let card = get(&app, &format!("/v1/resources/{genuine}")).await;
    assert_eq!(card["group"]["id"], group.as_str());
    assert_eq!(
        card["group"]["members"],
        json!([{"id": publisher, "uri": landing, "site": "www.nature.com"}]),
        "{card}"
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn registry_trust_needs_the_records_own_edition_and_a_real_landing_host(pool: PgPool) {
    use pinakes_core::resolver::cards;
    // An editor lists another edition in the victim record's identifiers.
    let mut record: Value = serde_json::from_slice(include_bytes!(
        "fixtures/registry/openlibrary-9780140328721.json"
    ))
    .unwrap();
    record["ISBN:9780140328721"]["identifiers"]["openlibrary"] = json!(["OL7353617M", "OL999M"]);
    let ol = recorded_json(
        "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
        &serde_json::to_vec(&record).unwrap(),
    );
    let d = cards::parse_isbn("isbn:9780140328721", "9780140328721", &ol, 86400).unwrap();
    let targets: Vec<_> = d
        .group_keys
        .iter()
        .filter(|k| k.basis == "registry")
        .filter_map(|k| k.target.clone())
        .collect();
    assert!(
        targets.iter().all(|t| t.contains("/books/OL7353617M")),
        "{targets:?}"
    );
    assert!(!targets.is_empty());
    let isbn = store::ensure_resource(&pool, "isbn:9780140328721")
        .await
        .unwrap();
    store::save_document(&pool, &isbn, 0, &d).await.unwrap();
    let e2 = page(
        &pool,
        "https://openlibrary.org/books/OL999M",
        "Other edition",
        &[],
    )
    .await;
    assert_ne!(gid(&pool, &e2).await, gid(&pool, &isbn).await);
    let own = page(
        &pool,
        "https://openlibrary.org/books/OL7353617M",
        "Edition",
        &[],
    )
    .await;
    assert_eq!(gid(&pool, &own).await, gid(&pool, &isbn).await);

    // A registrant landing URL on any doi.org host is the proxy, not a landing page.
    let csl = recorded_json(
        "https://api.crossref.org/v1/works/x/transform",
        &serde_json::to_vec(&json!({"type":"article-journal","title":"Proxy",
            "resource":{"primary":{"URL":"https://www.doi.org/landing"}}}))
        .unwrap(),
    );
    let d = cards::parse_doi("doi:10.5555/proxy", &csl, 86400).unwrap();
    assert!(d.group_keys.iter().all(|k| k.basis != "registry"));
    // A stored document that carries the key anyway does not join: grouping re-checks stored keys.
    let card = store::ensure_resource(&pool, "doi:10.5555/proxy")
        .await
        .unwrap();
    let mut stored = ResourceMeta::new("doi:10.5555/proxy", "doi", 86400);
    stored.name = Some("Proxy".into());
    group_keys::add(&mut stored, "registry", "https://www.doi.org/landing");
    store::save_document(&pool, &card, 0, &stored)
        .await
        .unwrap();
    let landing = page(&pool, "https://www.doi.org/landing", "Proxy page", &[]).await;
    assert_ne!(gid(&pool, &landing).await, gid(&pool, &card).await);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM resource_group WHERE resource_id=$1 AND key LIKE 'cite:%'"
        )
        .bind(&card)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn lock_contention_retries_shortly_without_charging(pool: PgPool) {
    let (a, b) = pair(&pool, "lock", None).await;
    assert_eq!(gid(&pool, &a).await, gid(&pool, &b).await);
    // While another writer holds the group lock, retries stay short and uncharged.
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
}

#[test]
fn tenant_shapes_cover_user_prefixes_show_pages_and_podbean_sites() {
    let o = |u: &str| group_keys::origin(u).unwrap();
    // /calendar/u/{n}/ is a per-user prefix, never a shared tenant.
    let ical = "https://calendar.google.com/calendar/ical/alice%40example.org/public/basic.ics";
    assert_eq!(
        o(ical),
        o("https://calendar.google.com/calendar/u/0/ical/alice%40example.org/public/basic.ics")
    );
    assert_ne!(
        o("https://calendar.google.com/calendar/u/0/embed?src=alice@example.org"),
        o("https://calendar.google.com/calendar/u/0/embed?src=bob@example.org")
    );
    assert_eq!(
        o("https://calendar.google.com/calendar/u/0/embed?src=alice@example.org"),
        o("https://calendar.google.com/calendar/embed?src=alice@example.org&ctz=Europe/Zurich")
    );
    // Without a tenant, each URL is its own scope.
    assert_ne!(
        o("https://calendar.google.com/calendar/u/0/r/week"),
        o("https://calendar.google.com/calendar/u/0/r/day")
    );
    // anchor.fm: show pages scope by show, feeds by /s/{id}; the two never collide.
    assert_eq!(
        o("https://anchor.fm/alice"),
        o("https://anchor.fm/alice/episodes/Episode-1-e1")
    );
    assert_ne!(o("https://anchor.fm/alice"), o("https://anchor.fm/bob"));
    assert_eq!(
        o("https://anchor.fm/s/123abc/podcast/rss"),
        "anchor.fm/s/123abc"
    );
    assert_ne!(
        o("https://anchor.fm/s/123abc/podcast/rss"),
        o("https://anchor.fm/s")
    );
    // Hosted Podbean sites are subdomain tenants; the shared feed host keeps its path rule.
    assert_ne!(
        o("https://alice.podbean.com/e/one/"),
        o("https://bob.podbean.com/e/one/")
    );
    assert_ne!(
        o("https://feed.podbean.com/alice/feed.xml"),
        o("https://feed.podbean.com/bob/feed.xml")
    );
    // Every built-in rule is well formed.
    for (host, rule) in group_keys::TENANT_RULES.iter() {
        let valid = match rule {
            group_keys::TenantRule::PathSegment(n) => (1..=8).contains(n),
            group_keys::TenantRule::Path(rule) => rule.valid(),
            _ => true,
        };
        assert!(valid, "{host}");
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_book_and_its_open_library_edition_page_stay_two_cards(pool: PgPool) {
    use pinakes_core::resolver::cards;
    // The edition page is admitted first, complete (name and cover), with as many taggers.
    let uri = "https://openlibrary.org/books/OL7353617M";
    let edition = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some("Fantastic Mr. Fox by Roald Dahl | Open Library".into());
    d.image = Some("https://covers.openlibrary.org/b/id/6498519-L.jpg".into());
    d.set_type("WebPage");
    store::save_document(&pool, &edition, 0, &d).await.unwrap();
    tag(&pool, &edition, &"e".repeat(52), "books").await;
    let ol = recorded_json(
        "https://openlibrary.org/api/books?bibkeys=ISBN:9780140328721&format=json&jscmd=data",
        include_bytes!("fixtures/registry/openlibrary-9780140328721.json"),
    );
    let card = cards::parse_isbn("isbn:9780140328721", "9780140328721", &ol, 86400).unwrap();
    let isbn = store::ensure_resource(&pool, "isbn:9780140328721")
        .await
        .unwrap();
    store::save_document(&pool, &isbn, 0, &card).await.unwrap();
    tag(&pool, &isbn, &"i".repeat(52), "books").await;
    let group = gid(&pool, &isbn).await;
    assert_eq!(gid(&pool, &edition).await, group);
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    // One type each: the group's type is the first by name.
    let view = get(&app, &format!("/v1/groups/{group}")).await;
    assert_eq!(view["type"], "Book", "{view}");
    let names: Vec<(&str, &str)> = view["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["type"].as_str().unwrap(), m["name"].as_str().unwrap()))
        .collect();
    assert!(names.contains(&("Book", card.name.as_deref().unwrap())));
    assert!(names.contains(&("WebPage", "Fantastic Mr. Fox by Roald Dahl | Open Library")));
    // Collapsed, equal references: the member with the lower ID, as its own card.
    let rows = get(&app, "/v1/search?collapse=group&tags=books").await["items"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    let first = isbn.clone().min(edition.clone());
    assert_eq!(rows[0]["id"], first.as_str());
    assert_eq!(
        rows[0]["type"],
        if first == isbn { "Book" } else { "WebPage" }
    );
}
/// Two ISBN cards whose (editable) records name one Open Library edition page never join each
/// other: the edition key joins a card only to the page itself, once that page is indexed.
#[sqlx::test(migrations = "../../migrations")]
async fn edition_keys_join_only_through_the_page(pool: PgPool) {
    use pinakes_core::resolver::cards;
    let body = include_str!("fixtures/registry/openlibrary-9780140328721.json");
    let url = "https://openlibrary.org/api/books?format=json&jscmd=data";
    let mut ids = vec![];
    for isbn in ["9780140328721", "9781234567897"] {
        let ol = recorded_json(url, body.replace("9780140328721", isbn).as_bytes());
        let uri = format!("isbn:{isbn}");
        let card = cards::parse_isbn(&uri, isbn, &ol, 86400).unwrap();
        let id = store::ensure_resource(&pool, &uri).await.unwrap();
        store::save_document(&pool, &id, 0, &card).await.unwrap();
        ids.push(id);
    }
    assert_eq!(
        gid(&pool, &ids[0]).await,
        ids[0],
        "no page: the cards stay apart"
    );
    assert_eq!(gid(&pool, &ids[1]).await, ids[1]);
    let uri = "https://openlibrary.org/books/OL7353617M";
    let edition = store::ensure_resource(&pool, uri).await.unwrap();
    let mut d = ResourceMeta::new(uri, "web", 86400);
    d.name = Some("Fantastic Mr. Fox | Open Library".into());
    store::save_document(&pool, &edition, 0, &d).await.unwrap();
    let group = gid(&pool, &edition).await;
    assert_ne!(group, edition);
    assert_eq!(gid(&pool, &ids[0]).await, group);
    assert_eq!(gid(&pool, &ids[1]).await, group);
}

/// An automatic group keeps its ID as it grows: a member whose unit ID is smaller than every
/// other's joins without renaming the group.
#[sqlx::test(migrations = "../../migrations")]
async fn a_group_keeps_its_id_when_a_smaller_member_joins(pool: PgPool) {
    use pinakes_core::model::identity;
    let card_id = identity("doi:10.5555/stable").unwrap().0;
    let first = identity("https://example.org/first").unwrap().0;
    let smaller = (0..1000)
        .map(|n| format!("late{n}"))
        .find(|n| {
            identity(&format!("https://example.org/{n}")).unwrap().0
                < card_id.clone().min(first.clone())
        })
        .unwrap();
    let card = card(&pool, "stable", &["first", &smaller], None).await;
    save(&pool, "first", &["stable"], None).await;
    let group = gid(&pool, &card).await;
    assert_ne!(group, card);
    let (late, _) = save(&pool, &smaller, &["stable"], None).await;
    assert_eq!(gid(&pool, &late).await, group);
    assert_eq!(gid(&pool, &card).await, group);
}

/// A member that leaves a group named after it and later pairs again gets a new group: its
/// fresh name never takes the existing group's ID, so the two sets never merge without evidence.
#[sqlx::test(migrations = "../../migrations")]
async fn a_member_that_leaves_and_regroups_never_takes_its_old_groups_id(pool: PgPool) {
    use pinakes_core::model::identity;
    let id = |uri: String| identity(&uri).unwrap().0;
    let others = [
        id("doi:10.5555/old".into()),
        id("doi:10.5555/new".into()),
        id("https://example.org/stays".into()),
    ];
    let mover = (0..1000)
        .map(|n| format!("mover{n}"))
        .find(|n| {
            others
                .iter()
                .all(|o| id(format!("https://example.org/{n}")) < *o)
        })
        .unwrap();
    let old = card(&pool, "old", &[&mover, "stays"], None).await;
    let (moved, _) = save(&pool, &mover, &["old"], None).await;
    save(&pool, "stays", &["old"], None).await;
    let group = gid(&pool, &old).await;
    assert_eq!(gid(&pool, &moved).await, group);
    // It leaves: the group keeps its ID with the members that stay.
    save(&pool, &mover, &[], None).await;
    assert_eq!(gid(&pool, &old).await, group);
    assert_eq!(gid(&pool, &moved).await, moved);
    // It pairs again on another key: a new group, not a merge into the old one.
    let new = card(&pool, "new", &[&mover], None).await;
    save(&pool, &mover, &["new"], None).await;
    let paired = gid(&pool, &moved).await;
    assert_eq!(gid(&pool, &new).await, paired);
    assert_ne!(paired, group);
    let sizes: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM group_unit WHERE group_id IS NOT NULL GROUP BY group_id ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sizes, [2, 2]);
}

/// A key that alone exceeds the compatibility budget is parked: its units keep their groups, the
/// run settles (planning the same input again would park it again) and counts it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_exhausted_run_keeps_established_groups(pool: PgPool) {
    let card = card(&pool, "budget", &["b1", "b2", "b3"], None).await;
    let mut pages = vec![];
    for p in ["b1", "b2", "b3"] {
        pages.push(save(&pool, p, &["budget"], None).await.0);
    }
    let group = gid(&pool, &card).await;
    assert_ne!(group, card);
    let tight = groups::GroupConfig {
        check_budget: Some(1),
        ..Default::default()
    };
    sqlx::query("DELETE FROM group_input")
        .execute(&pool)
        .await
        .unwrap();
    groups::reconcile(&pool, &pages[0], &tight).await;
    groups::drain(&pool, &tight, 100).await.unwrap();
    for id in pages.iter().chain([&card]) {
        assert_eq!(gid(&pool, id).await, group, "{id}");
    }
    let parked: i32 =
        sqlx::query_scalar("SELECT parked_keys FROM group_input WHERE resource_id=$1")
            .bind(&pages[0])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(parked > 0);
    let retrying: i64 = sqlx::query_scalar("SELECT count(*) FROM group_dirty")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(retrying, 0);
}

/// `n` page names `{prefix}{k}` in the order of their resource IDs, and so of their `cite:` keys.
fn by_id(prefix: &str, n: usize) -> Vec<String> {
    let mut names: Vec<(String, String)> = (0..200)
        .map(|k| format!("{prefix}{k}"))
        .map(|name| {
            let id = pinakes_core::model::identity(&format!("https://example.org/{name}"))
                .unwrap()
                .0;
            (id, name)
        })
        .collect();
    names.sort();
    names.into_iter().take(n).map(|(_, name)| name).collect()
}
/// Saves a page while its own grouping waits, so a later run with another config groups it.
async fn deferred(pool: &PgPool, name: &str, cites: &[&str], lat: Option<f64>) -> String {
    let uri = format!("https://example.org/{name}");
    let id = store::ensure_resource(pool, &uri).await.unwrap();
    sqlx::query("INSERT INTO group_dirty(resource_id,next_attempt_at) VALUES($1,now()+interval '1 day') ON CONFLICT(resource_id) DO UPDATE SET next_attempt_at=excluded.next_attempt_at")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    save(pool, name, cites, lat).await;
    sqlx::query("DELETE FROM group_dirty")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM group_input")
        .execute(pool)
        .await
        .unwrap();
    id
}
fn tight(budget: usize) -> groups::GroupConfig {
    groups::GroupConfig {
        check_budget: Some(budget),
        ..Default::default()
    }
}

/// A key over the budget never starves the keys after it: they still join the units it does not
/// touch, at the latest in the run that resumes where a stopped one ended.
#[sqlx::test(migrations = "../../migrations")]
async fn an_over_budget_key_never_starves_later_keys(pool: PgPool) {
    // The card's keys (`cite:{page}:…`) sort before the moving page's.
    let names = by_id("starve", 5);
    let (pages, mover) = (&names[..4], names[4].as_str());
    let named: Vec<&str> = names.iter().map(String::as_str).collect();
    let big = card(&pool, "ccc", &named, None).await;
    for p in &named {
        save(&pool, p, &["ccc"], None).await;
    }
    let group = gid(&pool, &big).await;
    // The last page now cites another card that names it, and is regrouped on a tight budget:
    // past the card's first page every key of the card alone exceeds it.
    let other = card(&pool, "ddd", &[mover], None).await;
    let moved = deferred(&pool, mover, &["ddd"], None).await;
    // The parked keys' checks count against the run: the first run stops before the moving
    // page's key, and the next one starts there.
    assert!(groups::reconcile(&pool, &moved, &tight(1)).await);
    assert_ne!(gid(&pool, &other).await, gid(&pool, &moved).await);
    sqlx::query("UPDATE group_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(groups::reconcile(&pool, &moved, &tight(1)).await);
    let pair = gid(&pool, &moved).await;
    assert_eq!(gid(&pool, &other).await, pair);
    assert_ne!(pair, group);
    for p in pages {
        let id = pinakes_core::model::identity(&format!("https://example.org/{p}"))
            .unwrap()
            .0;
        assert_eq!(gid(&pool, &id).await, group, "{p}");
    }
    assert_eq!(gid(&pool, &big).await, group);
    let parked: i32 =
        sqlx::query_scalar("SELECT parked_keys FROM group_input WHERE resource_id=$1")
            .bind(&moved)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(parked, 3);
}

/// A run too large for its budget stops, records the key it stopped at, and the next run starts
/// there: a join after the stop is made on the next run instead of never.
#[sqlx::test(migrations = "../../migrations")]
async fn a_stopped_run_resumes_where_it_stopped(pool: PgPool) {
    // Nine pages of one card (keys costing 1, 2, … 9 checks) sort before the moving page's key.
    let names = by_id("resume", 10);
    let (pages, mover) = (&names[..9], names[9].as_str());
    let named: Vec<&str> = names.iter().map(String::as_str).collect();
    let big = card(&pool, "hhh", &named, None).await;
    for p in pages {
        save(&pool, p, &["hhh"], None).await;
    }
    save(&pool, mover, &["hhh"], None).await;
    let group = gid(&pool, &big).await;
    // The moving page now cites another card; each key stays within a budget of nine checks,
    // all of them together exceed four budgets.
    let other = card(&pool, "iii", &[mover], None).await;
    let moved = deferred(&pool, mover, &["iii"], None).await;
    let resume_key = || async {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT resume_key FROM group_dirty WHERE resource_id=$1",
        )
        .bind(&moved)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(groups::reconcile(&pool, &moved, &tight(9)).await);
    let first = resume_key().await.expect("the run stopped");
    assert_ne!(gid(&pool, &other).await, gid(&pool, &moved).await);
    // An hour later the next run starts at that key and reaches the moving page's key.
    sqlx::query("UPDATE group_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(groups::reconcile(&pool, &moved, &tight(9)).await);
    let pair = gid(&pool, &moved).await;
    assert_eq!(gid(&pool, &other).await, pair);
    assert_ne!(pair, group);
    assert_ne!(resume_key().await, Some(first));
    assert_eq!(gid(&pool, &big).await, group);
}

/// A component that stops every run on unchanged input settles after `settle_after_stops` runs:
/// no more hourly replans until its input changes, and it is counted.
#[sqlx::test(migrations = "../../migrations")]
async fn a_component_that_always_stops_settles(pool: PgPool) {
    let names = by_id("settle", 10);
    let (pages, mover) = (&names[..9], names[9].as_str());
    let named: Vec<&str> = names.iter().map(String::as_str).collect();
    card(&pool, "jjj", &named, None).await;
    for p in pages {
        save(&pool, p, &["jjj"], None).await;
    }
    save(&pool, mover, &["jjj"], None).await;
    card(&pool, "kkk", &[mover], None).await;
    let moved = deferred(&pool, mover, &["kkk"], None).await;
    let cfg = groups::GroupConfig {
        check_budget: Some(9),
        settle_after_stops: 2,
        ..Default::default()
    };
    let state = || async {
        sqlx::query_as::<_, (Option<i32>, Option<bool>)>(
            "SELECT (SELECT stopped_runs FROM group_dirty WHERE resource_id=$1),(SELECT settled_stopped FROM group_input WHERE resource_id=$1)",
        )
        .bind(&moved)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(groups::reconcile(&pool, &moved, &cfg).await);
    assert_eq!(state().await, (Some(1), None));
    sqlx::query("UPDATE group_dirty SET next_attempt_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(groups::reconcile(&pool, &moved, &cfg).await);
    // The second stop on the same input settles it: no dirty row, the input recorded.
    assert_eq!(state().await, (None, Some(true)));
    let settled: i64 = sqlx::query_scalar("SELECT count(*) FROM group_input WHERE settled_stopped")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(settled, 1);
}

/// Past the budget a newcomer never joins a group through a finished key unchecked against the
/// members an unfinished key holds there.
#[sqlx::test(migrations = "../../migrations")]
async fn a_newcomer_never_joins_past_an_unfinished_key(pool: PgPool) {
    // Keys in order: the first page's, the newcomer's, then the far pages'.
    let names = by_id("join", 4);
    let (first, newcomer, far) = (&names[0], &names[1], &names[2..]);
    let named: Vec<&str> = names.iter().map(String::as_str).collect();
    let card = card(&pool, "eee", &named, None).await;
    save(&pool, first, &["eee"], None).await;
    for p in far {
        save(&pool, p, &["eee"], Some(47.0)).await;
    }
    let group = gid(&pool, &card).await;
    // The newcomer is far from the far pages: with every key checked it never joins.
    let id = deferred(&pool, newcomer, &["eee"], Some(10.0)).await;
    assert!(groups::reconcile(&pool, &id, &tight(2)).await);
    assert_eq!(gid(&pool, &id).await, id);
    assert_eq!(gid(&pool, &card).await, group);
}

/// Past the budget a chain whose bridging key is unfinished keeps every member in its group.
#[sqlx::test(migrations = "../../migrations")]
async fn a_chain_with_an_unfinished_bridge_stays_whole(pool: PgPool) {
    // `cite:{first}:…` sorts before `cite:{bridge}:…`; on the bridge the `fff` key before `ggg`.
    let names = by_id("chain", 2);
    let (first, bridge) = (names[0].as_str(), names[1].as_str());
    let left = card(&pool, "fff", &[first, bridge], None).await;
    let right = card(&pool, "ggg", &[bridge], None).await;
    let (a, _) = save(&pool, first, &["fff"], None).await;
    let (b, _) = save(&pool, bridge, &["fff", "ggg"], None).await;
    let group = gid(&pool, &left).await;
    for id in [&a, &b, &right] {
        assert_eq!(gid(&pool, id).await, group);
    }
    sqlx::query("DELETE FROM group_input")
        .execute(&pool)
        .await
        .unwrap();
    // The bridge's `fff` key costs two checks; the others one.
    assert!(groups::reconcile(&pool, &b, &tight(1)).await);
    for id in [&a, &b, &left, &right] {
        assert_eq!(gid(&pool, id).await, group, "{id}");
    }
}

/// A unit can leave a group and pair again any number of times: each new pair gets a fresh ID.
#[sqlx::test(migrations = "../../migrations")]
async fn a_unit_regroups_any_number_of_times(pool: PgPool) {
    use pinakes_core::model::identity;
    let id = |uri: String| identity(&uri).unwrap().0;
    let cards: Vec<String> = (0..20).map(|k| format!("cycle{k}")).collect();
    let card_ids: Vec<String> = cards
        .iter()
        .map(|c| id(format!("doi:10.5555/{c}")))
        .collect();
    // The mover has the smallest unit of every group it forms, so each is named after it.
    let mut pages = by_id("regroup", 21);
    let mover = pages.remove(0);
    let moved = id(format!("https://example.org/{mover}"));
    assert!(card_ids.iter().all(|c| moved < *c));
    let mut seen = std::collections::BTreeSet::new();
    for (k, (c, stays)) in cards.iter().zip(&pages).enumerate() {
        let held = card(&pool, c, &[&mover, stays], None).await;
        save(&pool, &mover, &[c], None).await;
        save(&pool, stays, &[c], None).await;
        let group = gid(&pool, &moved).await;
        assert_eq!(gid(&pool, &held).await, group, "cycle {k}");
        assert!(group != moved && seen.insert(group.clone()), "cycle {k}");
        // It leaves: the group keeps its ID with the members that stay.
        save(&pool, &mover, &[], None).await;
        assert_eq!(gid(&pool, &held).await, group, "cycle {k}");
        assert_eq!(gid(&pool, &moved).await, moved, "cycle {k}");
    }
}

/// Facets count links: collapsing changes the rows, not the counts.
#[sqlx::test(migrations = "../../migrations")]
async fn the_type_facet_counts_every_member_collapsed_or_not(pool: PgPool) {
    let (card, page) = pair(&pool, "facet", None).await;
    for id in [&card, &page] {
        tag(&pool, id, &"f".repeat(52), "facet").await;
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let separate = get(&app, "/v1/search?tags=facet&facet=type").await;
    let total = |v: &Value| {
        v["facets"]["type"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["count"].as_i64().unwrap())
            .sum::<i64>()
    };
    assert_eq!(total(&separate), 2, "{separate}");
    let collapsed = get(&app, "/v1/search?tags=facet&facet=type&collapse=group").await;
    assert_eq!(
        collapsed["items"].as_array().unwrap().len(),
        1,
        "{collapsed}"
    );
    assert_eq!(collapsed["facets"], separate["facets"]);
}

/// Orphan expiry removes a member's group rows with it: the group's size follows at once, the
/// survivors are reconciled, and a group left without evidence is dropped.
#[sqlx::test(migrations = "../../migrations")]
async fn an_expired_member_leaves_its_group(pool: PgPool) {
    let (card, page) = pair(&pool, "expiring", None).await;
    let group = gid(&pool, &card).await;
    assert_ne!(group, card);
    sqlx::query("UPDATE resource SET orphaned_at=now()-interval '30 days' WHERE id=$1")
        .bind(&page)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        pinakes_core::changes::delete_orphans(&pool, 7)
            .await
            .unwrap(),
        1
    );
    let count = |sql: &'static str| {
        let pool = pool.clone();
        let page = page.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(page)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    for sql in [
        "SELECT count(*) FROM group_unit WHERE resource_id=$1",
        "SELECT count(*) FROM resource_group WHERE resource_id=$1",
        "SELECT count(*) FROM group_override WHERE unit_id=$1",
    ] {
        assert_eq!(count(sql).await, 0, "{sql}");
    }
    let size: i64 = sqlx::query_scalar("SELECT count(*) FROM group_unit WHERE group_id=$1")
        .bind(&group)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(size <= 1, "{size}");
    groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    assert_eq!(gid(&pool, &card).await, card, "the survivor stands alone");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM \"group\" WHERE id=$1")
        .bind(&group)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Removing a resource's child row (here a tag) marks the resource dirty and bumps its revision;
/// a save holds the resource's row, then marks it dirty. The removal locks the resource's row
/// before marking it, as a save does, so the two never deadlock.
#[sqlx::test(migrations = "../../migrations")]
async fn removing_a_tag_and_saving_its_resource_take_locks_in_one_order(pool: PgPool) {
    let (id, _) = save(&pool, "child-order", &[], None).await;
    tag(&pool, &id, &"y".repeat(52), "first").await;
    sqlx::query("DELETE FROM group_dirty")
        .execute(&pool)
        .await
        .unwrap();
    // A save: the resource's row first.
    let mut saving = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
        .bind(&id)
        .execute(&mut *saving)
        .await
        .unwrap();
    let removal = tokio::spawn({
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query("DELETE FROM reference WHERE resource_id=$1")
                .bind(&id)
                .execute(&pool)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'DELETE FROM reference%')")
                .fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // The save's new document marks the resource dirty.
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{name}','\"Saved\"') WHERE id=$1")
        .bind(&id)
        .execute(&mut *saving)
        .await
        .unwrap();
    saving.commit().await.unwrap();
    removal.await.unwrap().unwrap();
    let dirty: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM group_dirty WHERE resource_id=$1)")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dirty);
}

/// Removing a unit from a group marks every member dirty; a save holds its member's row, then
/// marks it dirty. The removal locks the members' rows before marking them, as a save does, so
/// the two never deadlock.
#[sqlx::test(migrations = "../../migrations")]
async fn removing_a_group_unit_and_saving_a_member_take_locks_in_one_order(pool: PgPool) {
    let (card, page) = pair(&pool, "member-order", None).await;
    assert_eq!(gid(&pool, &card).await, gid(&pool, &page).await);
    sqlx::query("DELETE FROM group_dirty")
        .execute(&pool)
        .await
        .unwrap();
    // A save of the page: its row first.
    let mut saving = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM resource WHERE id=$1 FOR UPDATE")
        .bind(&page)
        .execute(&mut *saving)
        .await
        .unwrap();
    // A group run takes the card out of the group, which marks the page dirty too.
    let removal = tokio::spawn({
        let pool = pool.clone();
        let card = card.clone();
        async move {
            sqlx::query("DELETE FROM group_unit WHERE resource_id=$1")
                .bind(&card)
                .execute(&pool)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'DELETE FROM group_unit%')")
                .fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sqlx::query("UPDATE resource SET doc=jsonb_set(doc,'{name}','\"Saved\"') WHERE id=$1")
        .bind(&page)
        .execute(&mut *saving)
        .await
        .unwrap();
    saving.commit().await.unwrap();
    removal.await.unwrap().unwrap();
    // The member left behind is regrouped.
    let dirty: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM group_dirty WHERE resource_id=$1)")
            .bind(&page)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dirty);
}
