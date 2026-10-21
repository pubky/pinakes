use sqlx::PgPool;

/// `migrate` creates the schema once and then leaves the database as it is; a database built from
/// another version of `0001_schema.sql`, or from the numbered chain before it, is refused with the
/// remedy instead of being served.
#[sqlx::test(migrations = false)]
async fn a_database_from_another_schema_is_refused_with_the_remedy(pool: PgPool) {
    pinakes_core::migrate(&pool).await.unwrap();
    pinakes_core::migrate(&pool).await.unwrap();
    let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(applied, [1]);

    // An edited schema file.
    sqlx::query("UPDATE _sqlx_migrations SET checksum='\\x00'::bytea")
        .execute(&pool)
        .await
        .unwrap();
    let edited = pinakes_core::migrate(&pool).await.unwrap_err().to_string();
    assert!(
        edited.contains("another version of migrations/0001_schema.sql"),
        "{edited}"
    );
    assert!(edited.contains("recreate it"), "{edited}");

    // The numbered chain: versions the file no longer has.
    sqlx::query("DELETE FROM _sqlx_migrations")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time)
         SELECT v,'chain',true,'\\x00'::bytea,0 FROM generate_series(2,6) v",
    )
    .execute(&pool)
    .await
    .unwrap();
    let chain = pinakes_core::migrate(&pool).await.unwrap_err().to_string();
    assert!(chain.contains("recreate it"), "{chain}");
}
