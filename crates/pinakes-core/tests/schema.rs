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

/// The single-quoted values of a definition, sorted.
fn quoted(definition: &str) -> Vec<String> {
    let mut values: Vec<String> = definition
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect();
    values.sort();
    values
}
async fn constraint(pool: &PgPool, table: &str, name: &str) -> String {
    sqlx::query_scalar("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname=$2 AND conrelid=$1::regclass")
        .bind(table)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{table}.{name}: {e}"))
}
async fn function(pool: &PgPool, name: &str) -> String {
    sqlx::query_scalar("SELECT pg_get_functiondef($1::regproc)")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}
fn sorted<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut values: Vec<String> = values.into_iter().map(str::to_owned).collect();
    values.sort();
    values
}

/// A resource's route is one of the code's: the fetch routes and the card kinds, exactly.
#[sqlx::test(migrations = "../../migrations")]
async fn the_schema_allows_exactly_the_routes(pool: PgPool) {
    use pinakes_core::resolver::{cards::KINDS, ROUTES};
    assert_eq!(
        quoted(&constraint(&pool, "resource", "resource_resolver_check").await),
        sorted(ROUTES.into_iter().chain(KINDS))
    );
}

/// The trigger that starts a denial streak over names the classes the queue counts as denials,
/// exactly: a class only one side knew would grow a streak the trigger then zeroes, or zero one
/// the queue grows.
#[sqlx::test(migrations = "../../migrations")]
async fn the_denial_trigger_names_exactly_the_queue_s_denials(pool: PgPool) {
    let body = function(&pool, "reset_denial_streak").await;
    let list = body
        .split_once("NOT IN(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(list, _)| list)
        .unwrap_or_else(|| panic!("no NOT IN list: {body}"));
    assert_eq!(quoted(list), sorted(pinakes_core::queue::DENIAL_CLASSES));
}

/// The catalogues the schema accepts are `Catalogue::ALL`, written once (`is_catalogue`) and
/// checked by every table with a `catalogue` column.
#[sqlx::test(migrations = "../../migrations")]
async fn the_schema_allows_exactly_the_catalogues_once(pool: PgPool) {
    use pinakes_core::catalogue::Catalogue;
    assert_eq!(
        quoted(&function(&pool, "is_catalogue").await),
        sorted(Catalogue::ALL.map(Catalogue::name))
    );
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns WHERE table_schema='public' AND column_name='catalogue' AND table_name IN (SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE') ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for table in ["catalogue_card", "catalogue_map_cell", "catalogue_state"] {
        assert!(tables.iter().any(|t| t == table), "{table}: {tables:?}");
    }
    for table in &tables {
        assert_eq!(
            constraint(&pool, table, &format!("{table}_catalogue_check")).await,
            "CHECK (is_catalogue(catalogue))",
            "{table}"
        );
    }
}

/// Why grouping deferred a resource is one of the reasons the code writes, exactly: every
/// statement of the crate that writes `group_dirty` names its reason as a literal, and the
/// schema's list is those literals.
#[sqlx::test(migrations = "../../migrations")]
async fn the_schema_allows_exactly_the_grouping_deferrals(pool: PgPool) {
    fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push((
                    path.display().to_string(),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }
    let mut files = vec![];
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut written: Vec<String> = vec![];
    for (file, source) in &files {
        for line in source
            .lines()
            .filter(|l| l.contains("group_dirty") && l.contains("last_error="))
        {
            assert!(
                !line.contains("last_error=$") && !line.contains("last_error=excluded"),
                "{file}: a deferral reason not written as a literal: {line}"
            );
            written.extend(
                line.split("last_error='")
                    .skip(1)
                    .filter_map(|rest| rest.split_once('\'').map(|(value, _)| value.to_owned())),
            );
        }
    }
    written.sort();
    written.dedup();
    assert_eq!(
        quoted(&constraint(&pool, "group_dirty", "group_dirty_last_error_check").await),
        written
    );
}

/// A catalogue's operator text is cut to the 1 KiB its columns hold, at a character's edge: an
/// operator's long reason for a full import is stored, not refused.
#[sqlx::test(migrations = "../../migrations")]
async fn a_long_import_reason_is_cut_to_what_the_state_holds(pool: PgPool) {
    use pinakes_core::catalogue::{run, Catalogue};
    run::request_import(&pool, Catalogue::Wikidata, &"é".repeat(600))
        .await
        .unwrap();
    let stored: String = sqlx::query_scalar(
        "SELECT import_requested FROM catalogue_state WHERE catalogue='wikidata'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, "é".repeat(512));
    // Whatever writes them, the three hold at most 1 KiB.
    for column in ["import_requested", "waiting", "last_error"] {
        let refused = sqlx::query(&format!(
            "UPDATE catalogue_state SET {column}=repeat('x',1025) WHERE catalogue='wikidata'"
        ))
        .execute(&pool)
        .await
        .unwrap_err()
        .to_string();
        assert!(
            refused.contains(&format!("catalogue_state_{column}_check")),
            "{column}: {refused}"
        );
    }
}

/// A column named for an instant (`…_at`) holds one, as a `timestamptz`: never a number whose
/// unit only the code knows.
#[sqlx::test(migrations = "../../migrations")]
async fn every_instant_column_is_a_timestamp(pool: PgPool) {
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT table_name||'.'||column_name||' '||data_type FROM information_schema.columns WHERE table_schema='public' AND column_name LIKE '%\\_at' AND data_type<>'timestamp with time zone' AND table_name IN (SELECT table_name FROM information_schema.tables WHERE table_schema='public' AND table_type='BASE TABLE') ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(found, Vec::<String>::new());
}

/// A release is a MusicBrainz ID, stored as a `uuid`; an OSM place's box is four edges, and each
/// of its versions fits a document's cap (8 KiB), however many the retention keeps.
#[sqlx::test(migrations = "../../migrations")]
async fn typed_values_are_stored_as_their_types(pool: PgPool) {
    let release: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns WHERE table_name='link_release' AND column_name='release'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(release, "uuid");
    let place = |bbox: &'static str, versions: String| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO osm_place(osm_type,osm_id,bbox,type,type_path,doc,fetched_at,refresh_at,status,fetch_doc,versions) VALUES('node',1,$1::float8[],'Place','{Place}','{}',now(),now(),'ok','{}',$2::jsonb)")
                .bind(bbox)
                .bind(versions)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    };
    let big = format!("{{\"a\":\"{}\"}}", "x".repeat(9000));
    let many = format!(
        "{{{}}}",
        (0..200)
            .map(|n| format!("\"v{n}\":\"{}\"", "x".repeat(8000)))
            .collect::<Vec<_>>()
            .join(",")
    );
    for (bbox, versions, check) in [
        ("{1,2,3}", "{}".to_owned(), "osm_place_bbox_check"),
        ("{1,2,3,4,5}", "{}".to_owned(), "osm_place_bbox_check"),
        ("{{1,2},{3,4}}", "{}".to_owned(), "osm_place_bbox_check"),
        ("{1,2,3,4}", "[]".to_owned(), "osm_place_versions_check"),
        ("{1,2,3,4}", big, "osm_place_versions_check"),
    ] {
        let refused = place(bbox, versions).await.unwrap_err();
        assert!(refused.contains(check), "{bbox}: {refused}");
    }
    place("{1,2,3,4}", many).await.unwrap();
}

/// A venue that a card or an occurrence names cannot be deleted: each names it by a key.
#[sqlx::test(migrations = "../../migrations")]
async fn a_named_venue_cannot_be_deleted(pool: PgPool) {
    sqlx::query("INSERT INTO resource(id,uri) VALUES(md5('venue'),'https://example.org/venue'),(md5('hall'),'https://example.org/hall'),(md5('event'),'https://example.org/event')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET venue_id=md5('venue') WHERE id=md5('event')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO occurrence(resource_id,uid,start_at,status,venue_id) VALUES(md5('event'),'u1',now(),'scheduled',md5('hall'))")
        .execute(&pool)
        .await
        .unwrap();
    for (venue, key) in [
        ("venue", "resource_venue_id_fkey"),
        ("hall", "occurrence_venue_id_fkey"),
    ] {
        let refused = sqlx::query("DELETE FROM resource WHERE id=md5($1)")
            .bind(venue)
            .execute(&pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains(key), "{venue}: {refused}");
    }
    let named = sqlx::query("UPDATE occurrence SET venue_id=md5('gone')")
        .execute(&pool)
        .await
        .unwrap_err()
        .to_string();
    assert!(named.contains("occurrence_venue_id_fkey"), "{named}");
}

/// Names say what they are: an index that backs no primary key or unique constraint ends in
/// `_idx`, and a column holding a time ends in `_at` (an instant), `_until` or `_since` (a bound),
/// or `_start` / `_end` (a span's ends). sqlx's own table is not ours to name.
#[sqlx::test(migrations = "../../migrations")]
async fn indexes_and_time_columns_are_named_for_what_they_are(pool: PgPool) {
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT i.relname::text FROM pg_class i JOIN pg_namespace s ON s.oid=i.relnamespace JOIN pg_index x ON x.indexrelid=i.oid
         WHERE s.nspname='public' AND i.relname !~ '_idx$'
           AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid=i.oid AND c.contype IN ('p','u'))
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes, Vec::<String>::new());
    let times: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname||'.'||a.attname FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace s ON s.oid=c.relnamespace
         WHERE s.nspname='public' AND c.relkind='r' AND c.relname<>'_sqlx_migrations' AND a.attnum>0 AND NOT a.attisdropped
           AND a.atttypid IN ('timestamptz'::regtype,'timestamp'::regtype,'date'::regtype)
           AND a.attname !~ '_(at|until|since|start|end)$'
         ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(times, Vec::<String>::new());
}
