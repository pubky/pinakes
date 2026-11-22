use sqlx::PgPool;

/// The view as it was written before its branches: one row per reference, the alias's root for
/// an alias, posts' own rows left out. The view must mean exactly this.
const JOINED: &str = "SELECT coalesce(a.root_id, f.resource_id) AS resource_id, f.author, f.object_path, f.kind, f.label, f.created_at \
    FROM reference f LEFT JOIN resource_alias a ON a.alias_id = f.resource_id WHERE f.kind <> 'post'";

async fn except_all(pool: &PgPool, left: &str, right: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM (({left}) EXCEPT ALL ({right})) d"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Direct, alias and post references, an alias of an alias, and rows that are the same in every
/// column the view shows (one reference per unit of a post): the view holds the same rows as the
/// join, each as many times.
#[sqlx::test(migrations = "../../migrations")]
async fn the_view_holds_each_reference_once_for_its_root(pool: PgPool) {
    let mut seed = pool.acquire().await.unwrap();
    for statement in [
        "SET session_replication_role = replica",
        "INSERT INTO resource(id,uri) SELECT md5(n),'https://example.org/'||n FROM unnest(ARRAY['plain','root','alias','other-alias','top','middle','bottom','post-root','post-alias','none']) n",
        "INSERT INTO resource_alias(alias_id,root_id,basis) SELECT md5(a),md5(r),b FROM (VALUES ('alias','root','canonical'),('other-alias','root','tracking'),('middle','top','canonical'),('bottom','middle','canonical'),('post-alias','post-root','canonical')) v(a,r,b)",
        // Two units of one post on `plain` and on `alias`: equal in every column the view shows.
        "INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key) SELECT md5(r),k,lpad(a,52,'y'),l,p,c::timestamptz,u FROM (VALUES \
           ('plain','embed','a','', '/pub/app/p/1', '2026-01-01', 'u1'),('plain','embed','a','', '/pub/app/p/1', '2026-01-01', 'u2'), \
           ('plain','tag','b','music','/pub/app/t/1','2026-01-02',''),('plain','post','a',NULL,'/pub/app/p/2','2026-01-03',''), \
           ('root','tag','c','music','/pub/app/t/2','2026-01-04',''), \
           ('alias','embed','a','', '/pub/app/p/3', '2026-01-05', 'u1'),('alias','embed','a','', '/pub/app/p/3', '2026-01-05', 'u2'), \
           ('alias','reply','d',NULL,'/pub/app/r/1','2026-01-06',''),('other-alias','tag','c','music','/pub/app/t/3','2026-01-07',''), \
           ('middle','tag','e','x','/pub/app/t/4','2026-01-08',''),('bottom','tag','e','x','/pub/app/t/5','2026-01-09',''), \
           ('post-alias','post','f',NULL,'/pub/app/p/4','2026-01-10',''),('post-root','post','f',NULL,'/pub/app/p/5','2026-01-11','')) v(r,k,a,l,p,c,u)",
        "RESET session_replication_role",
    ] {
        sqlx::query(statement).execute(&mut *seed).await.unwrap();
    }
    drop(seed);
    let view =
        "SELECT resource_id, author, object_path, kind, label, created_at FROM effective_reference";
    assert_eq!(except_all(&pool, view, JOINED).await, 0);
    assert_eq!(except_all(&pool, JOINED, view).await, 0);
    // Each link's rows, as many as it counts.
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT n,count(*) FROM effective_reference f JOIN unnest(ARRAY['plain','root','alias','other-alias','top','middle','bottom','post-root','post-alias','none']) n ON f.resource_id=md5(n) GROUP BY n ORDER BY n",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let counts: Vec<(&str, i64)> = counts.iter().map(|(id, n)| (id.as_str(), *n)).collect();
    // `plain`: two units and a tag, its post's own row left out; `root`: its tag, `alias`'s
    // four and `other-alias`'s tag; one step up for an alias of an alias; posts count nowhere.
    assert_eq!(
        counts,
        [("middle", 1), ("plain", 3), ("root", 5), ("top", 1)]
    );
    // The view's columns, in order and type, as the schema declares them.
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text,data_type::text FROM information_schema.columns WHERE table_name='effective_reference' ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let columns: Vec<(&str, &str)> = columns
        .iter()
        .map(|(c, t)| (c.as_str(), t.as_str()))
        .collect();
    assert_eq!(
        columns,
        [
            ("resource_id", "text"),
            ("author", "text"),
            ("object_path", "text"),
            ("kind", "text"),
            ("label", "text"),
            ("created_at", "timestamp with time zone"),
        ]
    );
}

/// What a statement did with `reference`: the rows its scans of the table produced or discarded
/// over every loop, and whether it scanned the table sequentially.
#[derive(Debug, Default)]
struct Work {
    rows: i64,
    seq: bool,
}
fn work(plan: &serde_json::Value) -> Work {
    fn walk(node: &serde_json::Value, w: &mut Work) {
        let n = |k: &str| node[k].as_f64().unwrap_or(0.0);
        if node["Relation Name"] == "reference" {
            w.rows += ((n("Actual Rows")
                + n("Rows Removed by Filter")
                + n("Rows Removed by Index Recheck"))
                * n("Actual Loops")) as i64;
            w.seq |= node["Node Type"] == "Seq Scan";
        }
        for child in node["Plans"].as_array().into_iter().flatten() {
            walk(child, w);
        }
    }
    let mut w = Work::default();
    walk(&plan[0]["Plan"], &mut w);
    w
}

/// 4,000 links with 5 references each (20,000, a fifth of them posts' own rows), 400 of the
/// links aliases of others, and 20 links only an alias's references keep, statistics taken: a probe of the view by resource, as the claim's
/// admission, the score update and the orphan flag make it (one link, an array of links, one
/// per row of `resource`), reads the probed links' references by index, never the table.
#[sqlx::test(migrations = "../../migrations")]
async fn a_probe_of_the_view_by_resource_reads_only_its_references(pool: PgPool) {
    let mut seed = pool.acquire().await.unwrap();
    for statement in [
        "SET session_replication_role = replica",
        "INSERT INTO resource(id,uri) SELECT md5('r'||n),'https://example.org/'||n FROM generate_series(1,4000) n",
        // Links 3601..4000 are aliases of links 1..400.
        "INSERT INTO resource_alias(alias_id,root_id,basis) SELECT md5('r'||(3600+n)),md5('r'||n),'canonical' FROM generate_series(1,400) n",
        "INSERT INTO reference(resource_id,kind,author,label,object_path,created_at) \
           SELECT md5('r'||n),(ARRAY['tag','reply','embed','post','tag'])[k],lpad((n*7+k)::text,52,'y'),CASE WHEN k IN (1,5) THEN 't'||(n%50) END, \
             '/pub/app/'||n||'/'||k,timestamptz '2026-01-01'+(n*5+k)*interval '1 minute' FROM generate_series(1,4000) n,generate_series(1,5) k",
        // 20 links nobody references themselves, each the root of a referenced alias.
        "INSERT INTO resource(id,uri) SELECT md5(p||n),'https://example.org/'||p||n FROM generate_series(1,20) n,unnest(ARRAY['b','ba']) p",
        "INSERT INTO resource_alias(alias_id,root_id,basis) SELECT md5('ba'||n),md5('b'||n),'canonical' FROM generate_series(1,20) n",
        "INSERT INTO reference(resource_id,kind,author,object_path,created_at) SELECT md5('ba'||n),'embed',lpad(n::text,52,'z'),'/pub/app/ba/'||n,'2026-03-01' FROM generate_series(1,20) n",
        "RESET session_replication_role",
        "ANALYZE",
    ] {
        sqlx::query(statement).execute(&mut *seed).await.unwrap();
    }
    drop(seed);
    let twenty = "ARRAY(SELECT md5('r'||n) FROM generate_series(1,20) n)";
    let probes = [
        // A root with an alias, a plain link, an alias, a link nobody references, a root only
        // its alias's references keep.
        "SELECT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=md5('r1'))".to_owned(),
        "SELECT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=md5('r1000'))".to_owned(),
        "SELECT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=md5('r3601'))".to_owned(),
        "SELECT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=md5('nobody'))".to_owned(),
        format!("SELECT resource_id,count(*),max(created_at) FROM effective_reference WHERE resource_id=ANY({twenty}) GROUP BY resource_id"),
        "SELECT EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=md5('b1'))".to_owned(),
        format!("SELECT r.id,EXISTS(SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id) FROM resource r WHERE r.id=ANY({twenty})"),
    ];
    for probe in probes {
        let plan: serde_json::Value =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {probe}"))
                .fetch_one(&pool)
                .await
                .unwrap();
        let w = work(&plan);
        // The 20 links hold 100 references and their aliases 100 more: a bound far below the
        // 20,000 a scan reads.
        assert!(!w.seq && w.rows <= 400, "{probe}: {w:?}\n{plan}");
    }
    // The orphan flag of `store::recompute_scores`, for the 20 roots without references of
    // their own: the view is probed once per root.
    let mut tx = pool.begin().await.unwrap();
    let plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, FORMAT JSON) UPDATE resource r SET orphaned_at = CASE WHEN EXISTS \
         (SELECT 1 FROM reference f WHERE f.resource_id=r.id) OR EXISTS \
         (SELECT 1 FROM effective_reference f WHERE f.resource_id=r.id) THEN NULL ELSE coalesce(orphaned_at, now()) END \
         WHERE r.id = ANY(ARRAY(SELECT md5('b'||n) FROM generate_series(1,20) n))",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let w = work(&plan);
    assert!(!w.seq && w.rows <= 400, "{w:?}\n{plan}");
}
