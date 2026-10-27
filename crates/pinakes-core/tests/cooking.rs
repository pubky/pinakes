//! Recipes: recorded recipe pages become Recipe cards with their time, yield, ingredients and
//! rating; search's `ingredient` finds them by whole words of their ingredients' names.
use axum::{body::Body, extract::ConnectInfo, http::Request, Router};
use http_body_util::BodyExt;
use pinakes_core::{
    api::{self, AppState},
    config::Config,
    model::ResourceMeta,
    resolver::{egress::Fetched, web},
    store,
};
use serde_json::Value;
use sqlx::PgPool;
use std::net::SocketAddr;
use tower::ServiceExt;
mod common;

const LASAGNE: &str = "https://www.bbcgoodfood.com/recipes/classic-lasagne-0";
const PASTA: &str = "https://www.budgetbytes.com/one-pot-creamy-pesto-chicken-pasta/";

fn extracted(uri: &str, file: &str) -> ResourceMeta {
    let body = std::fs::read(format!(
        "{}/tests/fixtures/web/recipes/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let fetched = Fetched {
        permanent_redirect: false,
        truncated: false,
        url: uri.parse().unwrap(),
        status: 200,
        content_type: "text/html".into(),
        body,
        location: None,
        retry_after_secs: 60,
        validators: Default::default(),
    };
    web::extract(uri, &fetched, 3600).unwrap().doc
}

#[test]
fn recipe_pages_become_recipes_with_their_facts() {
    // The recipe outranks the page's video (a script of its own) and its blog article (a node
    // of the same graph).
    let doc = extracted(LASAGNE, "bbcgoodfood-classic-lasagne.html");
    assert_eq!(doc.type_name, "Recipe");
    assert_eq!(doc.name.as_deref(), Some("Classic lasagne"));
    let a = &doc.attributes;
    assert_eq!(a["total_minutes"], "120");
    assert_eq!(a["recipe_yield"], "6");
    assert_eq!(a["cuisine"], "Italian");
    assert_eq!(a["recipe_category"], "Dinner; Lunch; Main course");
    assert_eq!(
        a["ingredients"],
        "olive oil; lean beef mince; prosciutto; passata; hot beef stock; nutmeg; fresh lasagne sheets; white sauce; mozzarella torn into thin strips"
    );
    assert!(!a.contains_key("rating"));

    let doc = extracted(PASTA, "budgetbytes-pesto-chicken-pasta.html");
    assert_eq!(doc.type_name, "Recipe");
    assert_eq!(
        doc.name.as_deref(),
        Some("One Pot Creamy Pesto Chicken Pasta")
    );
    let a = &doc.attributes;
    assert_eq!(a["total_minutes"], "25");
    assert_eq!(a["recipe_yield"], "4");
    assert_eq!(a["cuisine"], "American");
    assert_eq!((&*a["rating"], &*a["rating_count"]), ("4.84", "254"));
    assert!(a["ingredients"].starts_with(
        "boneless, skinless chicken breast; butter; garlic; penne pasta; chicken broth; milk; cream cheese"
    ));
    let bounded = doc.clone().bounded(web::ATTRIBUTES).unwrap();
    assert_eq!(bounded.attributes, doc.attributes, "facts kept");
}

async fn call(app: &Router, path: &str) -> (u16, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:1111".parse::<SocketAddr>().unwrap()));
    let r = app.clone().oneshot(request).await.unwrap();
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status == 200 {
        common::check_response("GET", path.split('?').next().unwrap(), status, &value);
    }
    (status, value)
}
fn uris(v: &Value) -> Vec<&str> {
    let mut all: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["uri"].as_str().unwrap())
        .collect();
    all.sort();
    all
}

#[sqlx::test(migrations = "../../migrations", fixtures("fixtures/require_h3.sql"))]
async fn recipes_are_found_by_their_ingredients(pool: PgPool) {
    for (uri, file) in [
        (LASAGNE, "bbcgoodfood-classic-lasagne.html"),
        (PASTA, "budgetbytes-pesto-chicken-pasta.html"),
    ] {
        let doc = pinakes_core::resolver::html::off_runtime(|| extracted(uri, file));
        let id = store::ensure_resource(&pool, uri).await.unwrap();
        store::save_document(&pool, &id, 0, &doc).await.unwrap();
        sqlx::query("INSERT INTO reference(resource_id,author,kind,label,object_path,created_at) VALUES($1,$2,'tag','food',$3,now())")
            .bind(&id)
            .bind("a".repeat(52))
            .bind(format!("/pub/app/tags/{id}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = api::router(AppState::new(pool.clone(), Config::default()).unwrap());
    for (query, expected) in [
        ("ingredient=mozzarella", vec![LASAGNE]),
        // Whole words: "pasta" is an ingredient word of the pasta only ("penne pasta").
        ("ingredient=pasta", vec![PASTA]),
        ("ingredient=PESTO", vec![PASTA]),
        ("ingredient=beef%20stock", vec![LASAGNE]),
        // Every ingredient given must match.
        ("ingredient=garlic,milk", vec![PASTA]),
        ("ingredient=garlic,nutmeg", vec![]),
        // A part of a word is no match.
        ("ingredient=mozz", vec![]),
        ("type=Recipe", vec![PASTA, LASAGNE]),
    ] {
        let (code, v) = call(&app, &format!("/v1/search?{query}")).await;
        assert_eq!(code, 200, "{query}: {v}");
        let mut expected = expected;
        expected.sort();
        assert_eq!(uris(&v), expected, "{query}");
    }
    let (_, card) = call(&app, "/v1/search?ingredient=nutmeg").await;
    let id = card["items"][0]["id"].as_str().unwrap().to_owned();
    let (_, detail) = call(&app, &format!("/v1/resources/{id}")).await;
    assert_eq!(detail["facts"]["total_minutes"], "120");
    for bad in ["a,b,c,d", "flour;drop", "", "%25"] {
        let (code, _) = call(&app, &format!("/v1/search?ingredient={bad}")).await;
        assert_eq!(code, 400, "{bad}");
    }
}
