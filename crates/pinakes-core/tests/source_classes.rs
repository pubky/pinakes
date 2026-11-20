//! The error classes a source's health may hold: one list in the code, and the schema allows
//! exactly that list.
use pinakes_core::{
    resolver::{self, egress::BodyTooLarge, html::TooComplex, ErrorClass, RetryLater},
    source,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn the_schema_allows_exactly_the_source_error_classes(pool: PgPool) {
    let definition: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='source_error_class' AND conrelid='source_state'::regclass",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut schema: Vec<&str> = definition.split('\'').skip(1).step_by(2).collect();
    schema.sort_unstable();
    let mut code = source::ERROR_CLASSES.to_vec();
    code.sort_unstable();
    assert_eq!(schema, code, "{definition}");
}

/// Every class a resolver's failure is stored as for its source is one the schema allows.
#[test]
fn a_failure_is_stored_as_a_source_error_class() {
    let failures: Vec<anyhow::Error> = vec![
        BodyTooLarge { cap: 1024 }.into(),
        TooComplex {
            reason: "the CPU budget",
        }
        .into(),
        ErrorClass("dns").into(),
        ErrorClass("connect").into(),
        ErrorClass("http_4xx").into(),
        ErrorClass("http_5xx").into(),
        ErrorClass("policy").into(),
        RetryLater {
            class: "timeout",
            seconds: 60,
        }
        .into(),
        anyhow::anyhow!("an unreadable answer"),
    ];
    for failure in failures {
        let class = resolver::error_class(&failure);
        assert!(source::ERROR_CLASSES.contains(&class), "{class}");
    }
}

/// A redirect an upstream answered is stored on a link as `http_3xx`; a source's health, whose
/// classes the schema fixes, records it as the client-side error it is (`http_4xx`), never as
/// `parse`.
#[sqlx::test(migrations = "../../migrations")]
async fn a_redirect_is_a_client_error_in_a_source_s_health(pool: PgPool) {
    for (class, stored) in [("http_3xx", "http_4xx"), ("http_4xx", "http_4xx")] {
        source::outcome(&pool, "osm", Some(class)).await.unwrap();
        let last: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM source_state WHERE source='osm'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(last.as_deref(), Some(stored), "{class}");
    }
}
