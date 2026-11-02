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

fn api(id: u64) -> String {
    format!("https://api.stackexchange.com/2.3/questions/{id}?site=stackoverflow&filter=!)4meWwcO75JCcyGuJACpWc-xLmMU")
}

/// Store each question's card, run the source's also-on half on it and reconcile its group;
/// each is tagged once. Their ids, in order.
async fn recorded(
    pool: &PgPool,
    cx: &also_on::Context,
    questions: Vec<(&str, ResourceMeta)>,
) -> Vec<String> {
    let mut ids = vec![];
    for (uri, doc) in questions {
        let id = store::ensure_resource(pool, uri).await.unwrap();
        store::save_document(pool, &id, 0, &doc).await.unwrap();
        let s = subject(uri, Some("Question"), Some(doc));
        assert!(StackExchange.applies(&s));
        let found = StackExchange.lookup(cx, &s).await.unwrap();
        also_on::record(
            pool,
            &id,
            "stack_exchange",
            found,
            std::time::Duration::from_secs(86400),
            &AlsoOnConfig::default(),
        )
        .await
        .unwrap();
        pinakes_core::groups::reconcile(pool, &id, &Default::default()).await;
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','qa',$3,now())")
            .bind(&id)
            .bind("a".repeat(52))
            .bind(format!("/pub/app/tags/{id}"))
            .execute(pool)
            .await
            .unwrap();
        ids.push(id);
    }
    pinakes_core::groups::drain(pool, &Default::default(), 100)
        .await
        .unwrap();
    ids
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_duplicate_joins_its_original(pool: PgPool) {
    // The duplicate's lookup asks whether its original repeats another in turn: it does not.
    let original = std::fs::read_to_string(format!(
        "{}/tests/fixtures/sources/stack_exchange/se-stackoverflow-509211.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let cx = cx(
        Recorded::with_bodies(&[(api(509211).as_str(), "application/json", original)]),
        Config::default(),
    );
    let questions = [
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
    ]
    .map(|(uri, file)| (uri, question(uri, file)));
    let ids = recorded(&pool, &cx, Vec::from(questions)).await;
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
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

/// The API's answer for question `id` on Stack Overflow, closed as a duplicate of `original`.
fn answer(id: u64, original: Option<u64>) -> String {
    let mut q = serde_json::json!({"question_id": id, "title": format!("Question {id}"),
        "answer_count": 1, "score": 1, "tags": ["rust"], "creation_date": 1700000000});
    if let Some(original) = original {
        q["closed_reason"] = "Duplicate".into();
        q["closed_details"] =
            serde_json::json!({"original_questions": [{"question_id": original}]});
    }
    serde_json::json!({"items": [q], "has_more": false}).to_string()
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn a_chain_of_duplicates_is_one_group(pool: PgPool) {
    // A repeats B, which repeats C: all three carry C's key (the chain's end), so A joins B.
    let (a, b, c) = (1001, 1002, 1003);
    let cx = cx(
        Recorded::with_bodies(&[
            (api(b).as_str(), "application/json", answer(b, Some(c))),
            (api(c).as_str(), "application/json", answer(c, None)),
        ]),
        Config::default(),
    );
    let uri = |id| format!("https://stackoverflow.com/questions/{id}");
    let uris = [uri(a), uri(b), uri(c)];
    let questions = [(a, Some(b)), (b, Some(c)), (c, None)]
        .into_iter()
        .zip(&uris)
        .map(|((id, original), uri)| {
            let target = of_uri(uri).unwrap();
            let doc = card(
                ResourceMeta::new(uri, "stack_exchange", 3600),
                &target,
                200,
                answer(id, original).as_bytes(),
            )
            .unwrap()
            .unwrap();
            (uri.as_str(), doc)
        })
        .collect();
    let ids = recorded(&pool, &cx, questions).await;
    let app = api::router(
        AppState::new(pool.clone(), Config::default())
            .await
            .unwrap(),
    );
    for id in &ids {
        let card: Value = get(&app, &format!("/v1/resources/{id}")).await;
        assert_eq!(card["group"]["size"], 3, "{card}");
    }
    let found: Value = get(&app, "/v1/search?key=stackexchange:stackoverflow/1003").await;
    assert_eq!(found["items"].as_array().unwrap().len(), 3, "{found}");
}

#[tokio::test]
async fn a_chain_of_duplicates_ends_at_a_loop_or_after_three_hops() {
    // The key a duplicate carries, and the questions asked for it: a loop (A of B, B of A) ends
    // at B, asked once; a chain A, B, C, D, E, F ends three hops past B, at E, never asking E.
    let key = |chain: &[u64], bodies: Vec<(String, String)>| {
        let chain = chain.to_vec();
        async move {
            let transport = Recorded::with_bodies(
                &bodies
                    .iter()
                    .map(|(url, body)| (url.as_str(), "application/json", body.clone()))
                    .collect::<Vec<_>>(),
            );
            let cx = cx(transport.clone(), Config::default());
            let uri = format!("https://stackoverflow.com/questions/{}", chain[0]);
            let doc = card(
                ResourceMeta::new(&uri, "stack_exchange", 3600),
                &of_uri(&uri).unwrap(),
                200,
                answer(chain[0], Some(chain[1])).as_bytes(),
            )
            .unwrap()
            .unwrap();
            let s = subject(&uri, Some("Question"), Some(doc));
            let found = StackExchange.lookup(&cx, &s).await.unwrap();
            let keys: Vec<String> = found.keys.into_iter().map(|k| k.key).collect();
            let seen = transport.seen.lock().unwrap().clone();
            (keys, seen)
        }
    };
    let (a, b) = (2001, 2002);
    let (keys, seen) = key(&[a, b], vec![(api(b), answer(b, Some(a)))]).await;
    assert_eq!(keys, ["stackexchange:stackoverflow/2002"]);
    assert_eq!(seen, [api(b)]);

    let chain = [3001, 3002, 3003, 3004, 3005, 3006];
    let bodies = chain
        .windows(2)
        .map(|w| (api(w[0]), answer(w[0], Some(w[1]))))
        .collect();
    let (keys, seen) = key(&chain, bodies).await;
    assert_eq!(keys, ["stackexchange:stackoverflow/3005"]);
    assert_eq!(seen, [api(3002), api(3003), api(3004)]);
}
