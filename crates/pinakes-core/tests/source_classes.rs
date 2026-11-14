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
