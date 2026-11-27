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
    // It names where the way on is described, and no command that drops the database.
    assert!(edited.contains("docs/operations.md (Database)"), "{edited}");
    assert!(!edited.contains("dropdb"), "{edited}");

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
    assert!(chain.contains("docs/operations.md (Database)"), "{chain}");
}

/// No index repeats the leading keys of another: a plain b-tree index whose keys (in their order,
/// direction, collation and operator class) begin another plain b-tree index of the same table
/// serves no lookup the longer one cannot, and costs every write. Unique and partial indexes, and
/// those on expressions, are left to their own reasons.
#[sqlx::test(migrations = "../../migrations")]
async fn no_index_is_the_leading_part_of_another(pool: PgPool) {
    let found: Vec<String> = sqlx::query_scalar(
        "WITH ix AS (
           SELECT c.relname AS name, i.indrelid, i.indisunique AS uniq, i.indnkeyatts AS n,
             string_to_array(i.indkey::text, ' ')::int[] AS keys,
             string_to_array(i.indoption::text, ' ')::int[] AS options,
             string_to_array(i.indcollation::text, ' ')::oid[] AS collations,
             string_to_array(i.indclass::text, ' ')::oid[] AS classes
           FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_am am ON am.oid = c.relam
           JOIN pg_namespace s ON s.oid = c.relnamespace
           WHERE s.nspname = 'public' AND am.amname = 'btree' AND i.indpred IS NULL AND i.indexprs IS NULL)
         SELECT a.name || ' begins ' || b.name FROM ix a JOIN ix b ON b.indrelid = a.indrelid AND b.name <> a.name
         WHERE NOT a.uniq AND a.n <= b.n AND a.keys[1:a.n] = b.keys[1:a.n] AND a.options[1:a.n] = b.options[1:a.n]
           AND a.collations[1:a.n] = b.collations[1:a.n] AND a.classes[1:a.n] = b.classes[1:a.n]
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(found, Vec::<String>::new());
}

/// A unit's keys hold no copy of its resource or group: those are its `group_unit` row's, which
/// grouping moves, so a key row can never name another group than its unit's.
#[sqlx::test(migrations = "../../migrations")]
async fn a_unit_s_keys_hold_no_copy_of_its_resource_or_group(pool: PgPool) {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns WHERE table_schema='public' AND table_name='resource_group' ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(columns, ["unit_id", "key", "admitted_at"]);
}
