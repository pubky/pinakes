//! The `stack_exchange` source's also-on half: a question closed as a duplicate of another is in
//! its original's group, found by `key=stackexchange:{site}/{id}`.
mod common;

use common::media::*;
use pinakes_core::{
    also_on::{self, AlsoOnConfig, Source},
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    sources::stack_exchange::{also_on::StackExchange, card, of_uri},
    store,
};
use serde_json::Value;
use sqlx::PgPool;

fn question(uri: &str, file: &str) -> ResourceMeta {
    let path = format!(
        "{}/tests/fixtures/sources/stack_exchange/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let target = of_uri(uri).unwrap();
    card(
        ResourceMeta::new(&target.root(), "stack_exchange", 3600),
        &target,
        200,
        &std::fs::read(path).unwrap(),
    )
    .unwrap()
    .unwrap()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_duplicate_joins_its_original(pool: PgPool) {
    let cx = cx(Recorded::new(), Config::default());
    let mut ids = vec![];
    for (uri, file) in [
        (
            "https://stackoverflow.com/questions/663171",
            "se-stackoverflow-663171.json",
        ),
        (
            "https://stackoverflow.com/questions/509211",
            "se-stackoverflow-509211.json",
        ),
        // An unrelated question stays apart.
        (
            "https://stackoverflow.com/questions/11227809",
            "se-stackoverflow-11227809.json",
        ),
    ] {
        let doc = question(uri, file);
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        let s = subject(uri, Some("Question"), Some(doc));
        assert!(StackExchange.applies(&s));
        let found = StackExchange.lookup(&cx, &s).await.unwrap();
        also_on::record(
            &pool,
            &id,
            "stack_exchange",
            found,
            std::time::Duration::from_secs(86400),
            &AlsoOnConfig::default(),
        )
        .await
        .unwrap();
        pinakes_core::groups::reconcile(&pool, &id, &Default::default()).await;
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','qa',$3,now())")
            .bind(&id)
            .bind("a".repeat(52))
            .bind(format!("/pub/app/tags/{id}"))
            .execute(&pool)
            .await
            .unwrap();
        ids.push(id);
    }
    pinakes_core::groups::drain(&pool, &Default::default(), 100)
        .await
        .unwrap();
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    let duplicate: Value = get(&app, &format!("/v1/resources/{}", ids[0])).await;
    assert_eq!(duplicate["group"]["size"], 2, "{duplicate}");
    assert!(duplicate["also_on"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == ids[1].as_str()));
    assert_eq!(
        duplicate["facts"]["duplicate_of"],
        "https://stackoverflow.com/questions/509211"
    );
    let other: Value = get(&app, &format!("/v1/resources/{}", ids[2])).await;
    assert!(other["group"].is_null());
    let found: Value = get(&app, "/v1/search?key=stackexchange:stackoverflow/509211").await;
    let mut listed: Vec<&str> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    listed.sort();
    let mut expected = vec![ids[0].as_str(), ids[1].as_str()];
    expected.sort();
    assert_eq!(listed, expected);
}

async fn get(app: &axum::Router, path: &str) -> Value {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:1111".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let r = app.clone().oneshot(request).await.unwrap();
    assert_eq!(r.status(), 200, "{path}");
    serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
