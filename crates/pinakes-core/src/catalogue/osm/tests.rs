//! The importer through the runner's own step, on small files: a full import (two passes, the
//! node index, relations' geometry), an update from a change file (changed cards only,
//! removals, successors, renames, shared links following a successor), a resumed import, and a
//! real extract's part.
use super::pbf::tests::{data_block, file, tempfile_dir, Element::*};
use super::*;
use crate::catalogue::run::{self as runner, Context};
use crate::catalogue::store;
use sqlx::PgPool;
use std::sync::Arc;

fn config(dir: &Path) -> Arc<crate::config::Config> {
    let mut config = crate::config::Config::default();
    config.catalogues.dir = dir.display().to_string();
    config.catalogues.osm.region = "europe/liechtenstein".into();
    Arc::new(config)
}
async fn step(
    pool: &PgPool,
    dir: &Path,
    run: Run,
    files: Vec<PathBuf>,
    dense: bool,
) -> runner::Next {
    let cfg = (*config(dir)).clone();
    let importer = OsmImporter {
        input: Input::Files(files),
        region: cfg.catalogues.osm.region.clone(),
        force: Some(run),
        layout: dense.then_some(nodes::Layout::Dense),
        upstream: None,
        index_cap: None,
        threads: 2,
    };
    let mut ctx = Context::new(pool.clone(), Arc::new(cfg));
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    runner::step(&importer, &ctx).await.unwrap()
}
async fn hashes(pool: &PgPool) -> std::collections::BTreeMap<String, Vec<u8>> {
    sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT source_id,content_hash FROM catalogue_card WHERE catalogue='osm'",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect()
}
async fn name(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT name FROM catalogue_card WHERE catalogue='osm' AND source_id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// A small region: a café, an ATM, a stop position (left out), two restaurants, a shop drawn as
/// a way and a park relation around it.
fn region() -> Vec<u8> {
    let header = pbf::Header {
        required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
        optional_features: vec!["Sort.Type_then_ID".into()],
        replication_timestamp: Some(1_764_000_000),
        replication_sequence: Some(4978),
        replication_base_url: Some(
            "https://download.geofabrik.de/europe/liechtenstein/updates".into(),
        ),
    };
    file(
        &header,
        &[
            data_block(&[
                Node(
                    1,
                    47.1410,
                    9.5215,
                    &[("amenity", "cafe"), ("name", "Engel"), ("wikidata", "Q1")],
                ),
                Node(
                    2,
                    47.1411,
                    9.5216,
                    &[("amenity", "atm"), ("operator", "LGT")],
                ),
                Node(3, 47.1000, 9.5000, &[]),
                Node(4, 47.1010, 9.5000, &[]),
            ]),
            data_block(&[
                Node(5, 47.1010, 9.5010, &[]),
                Node(6, 47.1000, 9.5010, &[]),
                Node(
                    7,
                    47.1200,
                    9.5200,
                    &[("public_transport", "stop_position"), ("name", "Post")],
                ),
                Node(
                    8,
                    47.1300,
                    9.5300,
                    &[
                        ("amenity", "restaurant"),
                        ("name", "Rössli"),
                        ("wikidata", "Q5"),
                    ],
                ),
                Node(
                    9,
                    47.1400,
                    9.5400,
                    &[("amenity", "restaurant"), ("name", "Löwen")],
                ),
            ]),
            data_block(&[Way(
                10,
                &[3, 4, 5, 6, 3],
                &[("shop", "supermarket"), ("name", "Migros")],
            )]),
            data_block(&[Relation(
                20,
                &[(pbf::Kind::Way, 10, "outer"), (pbf::Kind::Node, 1, "")],
                &[
                    ("type", "multipolygon"),
                    ("leisure", "park"),
                    ("name", "Stadtpark"),
                ],
            )]),
        ],
    )
}
const CHANGES: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<osmChange version="0.6">
  <modify>
    <node id="1" version="2" changeset="100" timestamp="2026-11-25T10:00:00Z" lat="47.1410" lon="9.5215">
      <tag k="amenity" v="fast_food"/><tag k="name" v="Burger King"/><tag k="wikidata" v="Q1"/>
    </node>
    <node id="2" version="2" changeset="100" timestamp="2026-11-25T10:00:00Z" lat="47.1411" lon="9.5216"/>
    <way id="10" version="2" changeset="101" timestamp="2026-11-25T11:00:00Z">
      <nd ref="3"/><nd ref="4"/><nd ref="5"/><nd ref="6"/><nd ref="3"/>
      <tag k="shop" v="supermarket"/><tag k="name" v="Migros Vaduz"/>
    </way>
  </modify>
  <delete>
    <node id="8" version="2" changeset="102" timestamp="2026-11-25T12:00:00Z"/>
    <node id="9" version="2" changeset="103" timestamp="2026-11-25T12:00:00Z"/>
  </delete>
  <create>
    <node id="11" version="1" changeset="103" timestamp="2026-11-25T12:00:00Z" lat="47.14005" lon="9.54005">
      <tag k="amenity" v="restaurant"/><tag k="name" v="Löwen"/>
    </node>
    <node id="12" version="1" changeset="104" timestamp="2026-11-25T13:00:00Z" lat="47.1305" lon="9.5305">
      <tag k="amenity" v="restaurant"/><tag k="name" v="Rössli Neu"/><tag k="wikidata" v="Q5"/>
    </node>
    <node id="13" version="1" changeset="104" lat="47.2" lon="9.6"/>
  </create>
</osmChange>"#;
fn gz(text: &str) -> Vec<u8> {
    let mut z = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    std::io::Write::write_all(&mut z, text.as_bytes()).unwrap();
    z.finish().unwrap()
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_import_reads_places_ways_and_relations(pool: PgPool) {
    for planet in [false, true] {
        let dir = tempfile_dir();
        let pbf = dir.join("region.osm.pbf");
        std::fs::write(&pbf, region()).unwrap();
        sqlx::query("DELETE FROM catalogue_card")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM catalogue_state")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            step(&pool, &dir, Run::Import, vec![pbf.clone()], planet).await,
            runner::Next::Again
        );
        let ids: Vec<String> = hashes(&pool).await.into_keys().collect();
        assert_eq!(
            ids,
            [
                "node/1",
                "node/2",
                "node/8",
                "node/9",
                "relation/20",
                "way/10"
            ],
            "planet {planet}"
        );
        assert_eq!(name(&pool, "node/2").await.as_deref(), Some("ATM · LGT"));
        let (lat, lon, bbox): (f64, f64, Value) = sqlx::query_as("SELECT ST_Y(point),ST_X(point),details->'bbox' FROM catalogue_card WHERE source_id='way/10'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            (lat - 47.1005).abs() < 1e-6 && (lon - 9.5005).abs() < 1e-6,
            "{lat} {lon}"
        );
        assert_eq!(bbox, json!([9.5, 47.1, 9.501, 47.101]));
        // The relation's box holds its way and its node.
        let (rlat, bbox): (f64, Value) = sqlx::query_as(
            "SELECT ST_Y(point),details->'bbox' FROM catalogue_card WHERE source_id='relation/20'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(bbox, json!([9.5, 47.1, 9.5215, 47.141]));
        assert!((rlat - 47.1205).abs() < 1e-6);
        let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
        assert_eq!(state.position.as_ref().unwrap()["sequence"], 4978);
        assert_eq!(
            state.position.as_ref().unwrap()["region"],
            "europe/liechtenstein"
        );
        assert_eq!(state.cursor, None);
        assert_eq!(
            state.current_as_of.map(|t| t.timestamp()),
            Some(1_764_000_000)
        );
        assert_eq!(state.cards, 6);
        // The temporary files are gone; the given file stays.
        assert!(!dir.join("osm/nodes.idx").exists() && !dir.join("osm/relations.jsonl").exists());
        assert!(pbf.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[sqlx::test(migrations = "../../migrations")]
async fn an_update_changes_only_what_the_file_changes(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    // Shared links of the two objects about to be deleted.
    let lowen = crate::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/9")
        .await
        .unwrap();
    let rossli = crate::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/8")
        .await
        .unwrap();
    let before = hashes(&pool).await;
    let changes = dir.join("1.osc.gz");
    std::fs::write(&changes, gz(CHANGES)).unwrap();
    assert_eq!(
        step(&pool, &dir, Run::Update, vec![changes.clone()], false).await,
        runner::Next::Again
    );
    let after = hashes(&pool).await;
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        ["node/1", "node/11", "node/12", "relation/20", "way/10"]
    );
    // Only the objects the file changed changed; the relation it does not name kept its card.
    assert_eq!(after["relation/20"], before["relation/20"]);
    assert_ne!(after["node/1"], before["node/1"]);
    assert_ne!(after["way/10"], before["way/10"]);
    assert_eq!(name(&pool, "way/10").await.as_deref(), Some("Migros Vaduz"));
    // A rename to an unrelated name is recorded; a small one is not.
    let renames: Vec<(String, i64, String, String)> =
        sqlx::query_as("SELECT osm_type,osm_id,old_name,new_name FROM osm_rename ORDER BY osm_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        renames,
        [("node".into(), 1, "Engel".into(), "Burger King".into())]
    );
    // Successors: the same changeset, name and place; the same Wikidata item.
    let successors: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT old_id,new_id,basis FROM osm_successor ORDER BY old_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        successors,
        [(8, 12, "wikidata".into()), (9, 11, "changeset".into())]
    );
    // Their shared links are aliases of the successors' links now.
    let roots: Vec<(String, String)> = sqlx::query_as("SELECT a.alias_id,r.uri FROM resource_alias a JOIN resource r ON r.id=a.root_id ORDER BY r.uri")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        roots,
        [
            (lowen, "https://www.openstreetmap.org/node/11".to_owned()),
            (rossli, "https://www.openstreetmap.org/node/12".to_owned())
        ]
    );
    // The same file again changes nothing.
    let again = hashes(&pool).await;
    step(&pool, &dir, Run::Update, vec![changes], false).await;
    assert_eq!(hashes(&pool).await, again);
    assert_eq!(
        store::state(&pool, Catalogue::Osm)
            .await
            .unwrap()
            .unwrap()
            .current_as_of
            .map(|t| t.to_rfc3339()),
        Some("2026-11-25T13:00:00+00:00".into())
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_cut_off_import_resumes_from_its_cursor(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    // A first pass cut off after its first block: index and set-aside files as it left them.
    let work = dir.join("osm");
    std::fs::create_dir_all(&work).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let (p, w) = (pbf.clone(), work.clone());
    let pass = tokio::task::spawn_blocking(move || {
        first_pass(
            &p,
            &w,
            nodes::Layout::Sparse,
            &Value::Null,
            1,
            None,
            Arc::new(|_| Ok(u64::MAX)),
            tx,
        )
    });
    let mut batches = vec![];
    while let Some(b) = rx.recv().await {
        batches.push(b);
    }
    pass.await.unwrap().unwrap();
    // Everything fit in one final batch here; resume from a cursor at the second block instead.
    let index_len = std::fs::metadata(work.join("nodes.idx")).unwrap().len();
    assert_eq!(index_len, 9 * 16);
    store::ensure_state(&pool, Catalogue::Osm).await.unwrap();
    let mut blocks = vec![];
    pbf::read_blocks(&pbf, 0, 0, 1, |d| {
        blocks.push((d.index, d.offset, d.next));
        Ok(true)
    })
    .unwrap();
    let cursor = json!({"stage":"pass1","blob":1,"offset":blocks[1].1,"index":4 * 16,"pending":0,"ways_at":null,"file":pbf.display().to_string(),"layout":"sparse"});
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',generation=1,cursor=$1")
        .bind(&cursor)
        .execute(&pool)
        .await
        .unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    // The first block's cards were written before the cut; the rest after it. Here the cut came
    // before any write: what the resumed run read is there, the first block's nodes in the index.
    let ids: Vec<String> = hashes(&pool).await.into_keys().collect();
    assert_eq!(ids, ["node/8", "node/9", "relation/20", "way/10"]);
    let (lat,): (f64,) =
        sqlx::query_as("SELECT ST_Y(point) FROM catalogue_card WHERE source_id='way/10'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        (lat - 47.1005).abs() < 1e-6,
        "the way's geometry uses nodes indexed before the cut"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[sqlx::test(migrations = "../../migrations")]
async fn a_real_extract_part_imports(pool: PgPool) {
    let dir = tempfile_dir();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/catalogues/osm/liechtenstein-part.osm.pbf");
    step(&pool, &dir, Run::Import, vec![fixture], false).await;
    let (cards, named, useful, ways, relations, boxed): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER (WHERE details->>'unnamed' IS NULL),count(*) FILTER (WHERE details->>'unnamed'='true'),\
         count(*) FILTER (WHERE source_id LIKE 'way/%'),count(*) FILTER (WHERE source_id LIKE 'relation/%'),count(*) FILTER (WHERE details ? 'bbox') FROM catalogue_card WHERE catalogue='osm'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        cards > 100 && named > 50 && useful > 0 && ways > 0 && relations > 0 && boxed > 0,
        "{cards} {named} {useful} {ways} {relations} {boxed}"
    );
    let bad: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_card WHERE catalogue='osm' AND (uri<>'https://www.openstreetmap.org/'||source_id OR NOT keys @> ARRAY['osm:'||source_id] OR details->>'category'='public_transport/stop_position')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bad, 0);
    // The real change file reads (its objects are mostly outside this part).
    let changes = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/catalogues/osm/liechtenstein-4978.osc.gz");
    let read = read_changes(
        std::fs::File::open(&changes).unwrap(),
        "fixture",
        MAX_CHANGES_HELD,
    )
    .await
    .unwrap();
    assert!(!read.changed.is_empty() && read.latest.is_some());
    // Held to a cap below what the file takes in memory: `size`.
    let failed = read_changes(std::fs::File::open(&changes).unwrap(), "fixture", 4096)
        .await
        .map(|_| ())
        .unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("size"),
        "{failed:#}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn replication_paths_and_states() {
    assert_eq!(sequence_path(4979), "000/004/979");
    assert_eq!(sequence_path(12_345_678), "012/345/678");
    let (seq, at) = parse_state("# original OSM minutely replication sequence number 7387014\ntimestamp=2026-11-24T21\\:21\\:23Z\nsequenceNumber=4979\n").unwrap();
    assert_eq!(
        (seq, at.to_rfc3339()),
        (4979, "2026-11-24T21:21:23+00:00".into())
    );
    assert!(parse_state("<html>").is_none());
    assert_eq!(
        sources("planet").1,
        "https://planet.openstreetmap.org/replication/day"
    );
    assert_eq!(
        sources("europe/liechtenstein"),
        (
            "https://download.geofabrik.de/europe/liechtenstein/liechtenstein-latest.osm.pbf"
                .into(),
            "https://download.geofabrik.de/europe/liechtenstein/updates".into()
        )
    );
}

/// A day's file holding several edits of one object applies its last one: a modify then a
/// delete removes the card; an untag then a retag keeps it.
#[sqlx::test(migrations = "../../migrations")]
async fn a_file_with_several_edits_of_an_object_applies_the_last(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let edits = r#"<osmChange version="0.6">
  <modify><node id="9" version="2" lat="47.14" lon="9.54"><tag k="amenity" v="restaurant"/><tag k="name" v="Löwen Neu"/></node></modify>
  <delete><node id="9" version="3"/></delete>
  <modify><node id="8" version="2" lat="47.13" lon="9.53"/></modify>
  <modify><node id="8" version="3" lat="47.13" lon="9.53"><tag k="amenity" v="restaurant"/><tag k="name" v="Rössli"/><tag k="wikidata" v="Q5"/></node></modify>
</osmChange>"#;
    let changes = dir.join("2.osc.gz");
    std::fs::write(&changes, gz(edits)).unwrap();
    step(&pool, &dir, Run::Update, vec![changes], false).await;
    assert_eq!(name(&pool, "node/9").await, None);
    assert_eq!(name(&pool, "node/8").await.as_deref(), Some("Rössli"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Short free disk once the need is known is a wait, not a failure: the run and its cursor stay.
#[sqlx::test(migrations = "../../migrations")]
async fn short_disk_makes_the_import_wait(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    let cfg = config(&dir);
    let importer = OsmImporter {
        input: Input::Files(vec![pbf]),
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Import),
        layout: None,
        upstream: None,
        index_cap: None,
        threads: 1,
    };
    let mut ctx = Context::new(pool.clone(), cfg);
    // Room for the runner's own check (none for a local file), not for the file's index.
    ctx.free_disk = Arc::new(|_| Ok(1));
    let next = runner::step(&importer, &ctx).await.unwrap();
    assert!(matches!(next, runner::Next::Wait(_)));
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(state.run.as_deref(), Some("import"));
    assert_eq!(state.error_count, 0);
    assert!(
        state
            .waiting
            .as_deref()
            .is_some_and(|w| w.starts_with("free disk")),
        "{:?}",
        state.waiting
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A deletion reverted: the object is back, its successor forgotten, and its shared link answers
/// for itself again.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reverted_deletion_is_followed_no_more(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let lowen = crate::store::ensure_resource(&pool, "https://www.openstreetmap.org/node/9")
        .await
        .unwrap();
    let changes = dir.join("1.osc.gz");
    std::fs::write(&changes, gz(CHANGES)).unwrap();
    step(&pool, &dir, Run::Update, vec![changes], false).await;
    let aliased: i64 = sqlx::query_scalar("SELECT count(*) FROM resource_alias WHERE alias_id=$1")
        .bind(&lowen)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(aliased, 1);
    let revert = r#"<osmChange version="0.6"><create><node id="9" version="4" lat="47.14" lon="9.54"><tag k="amenity" v="restaurant"/><tag k="name" v="Löwen"/></node></create></osmChange>"#;
    let back = dir.join("3.osc.gz");
    std::fs::write(&back, gz(revert)).unwrap();
    step(&pool, &dir, Run::Update, vec![back], false).await;
    let (successors, aliased): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM osm_successor WHERE old_id=9),(SELECT count(*) FROM resource_alias WHERE alias_id=$1)")
        .bind(&lowen)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((successors, aliased), (0, 0));
    let (resolver, osm_id): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT resolver,osm_id FROM resource WHERE id=$1")
            .bind(&lowen)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((resolver.as_deref(), osm_id), (Some("osm"), Some(9)));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A local replication server: `state.txt`, and each change file and its state by path. Counts
/// the requests.
async fn replication(
    files: Vec<(&'static str, Vec<u8>)>,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/updates", listener.local_addr().unwrap());
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = asked.clone();
    let files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (files, counted) = (files.clone(), counted.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = files
                    .iter()
                    .find(|(p, _)| format!("/updates/{p}") == path)
                    .map(|(_, b)| b.clone());
                let head = match &body {
                    Some(b) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"x\"\r\nConnection: close\r\n\r\n", b.len()),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                };
                let _ = socket.write_all(head.as_bytes()).await;
                if let Some(b) = body {
                    let _ = socket.write_all(&b).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, asked)
}

/// The daemon's update path: the server's state, the change files after the stored sequence,
/// each file's state for the time; nothing new is a finished update; a file gone asks for a full
/// import.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_reads_the_change_files_after_its_sequence(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    sqlx::query("UPDATE catalogue_state SET position=jsonb_set(position,'{sequence}','4978')")
        .execute(&pool)
        .await
        .unwrap();
    let state = "sequenceNumber=4979\ntimestamp=2026-11-25T14\\:00\\:00Z\n";
    let (base, asked) = replication(vec![
        ("state.txt", state.as_bytes().to_vec()),
        ("000/004/979.osc.gz", gz(CHANGES)),
        ("000/004/979.state.txt", state.as_bytes().to_vec()),
    ])
    .await;
    let mut cfg = (*config(&dir)).clone();
    cfg.http.contact = "test@example.org".into();
    cfg.egress.contact = "test@example.org".into();
    let cfg = Arc::new(cfg);
    let importer = OsmImporter {
        input: Input::Region,
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Update),
        layout: None,
        upstream: Some((format!("{base}/none.osm.pbf"), base.clone())),
        index_cap: None,
        threads: 1,
    };
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    assert_eq!(
        runner::step(&importer, &ctx).await.unwrap(),
        runner::Next::Again
    );
    assert_eq!(name(&pool, "node/1").await.as_deref(), Some("Burger King"));
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(state.position.as_ref().unwrap()["sequence"], 4979);
    assert_eq!(
        state.current_as_of.map(|t| t.to_rfc3339()),
        Some("2026-11-25T14:00:00+00:00".into())
    );
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 3);
    // Nothing new: one request, a finished update.
    assert_eq!(
        runner::step(&importer, &ctx).await.unwrap(),
        runner::Next::Again
    );
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 4);
    // A file missing just behind the server's newest: tried again, nothing asked for.
    let (behind, _) = replication(vec![(
        "state.txt",
        b"sequenceNumber=4981\ntimestamp=2026-11-27T00\\:00\\:00Z\n".to_vec(),
    )])
    .await;
    let importer = OsmImporter {
        upstream: Some((format!("{behind}/none.osm.pbf"), behind)),
        ..importer
    };
    assert!(runner::step(&importer, &ctx).await.is_err());
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(state.import_requested, None);
    sqlx::query("UPDATE catalogue_state SET run=NULL,cursor=NULL,phase='idle'")
        .execute(&pool)
        .await
        .unwrap();
    // A file gone well behind it: a full import is asked for.
    let (gone, _) = replication(vec![(
        "state.txt",
        b"sequenceNumber=4990\ntimestamp=2026-11-30T00\\:00\\:00Z\n".to_vec(),
    )])
    .await;
    let importer = OsmImporter {
        upstream: Some((format!("{gone}/none.osm.pbf"), gone)),
        ..importer
    };
    runner::step(&importer, &ctx).await.unwrap();
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(
        state.import_requested.as_deref(),
        Some("a change file is gone")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// The daemon runs the OSM importer while the catalogue is on and the operator's contact is set,
/// and only then; without the contact the state row says why.
#[sqlx::test(migrations = "../../migrations")]
async fn the_daemon_runs_the_importer_with_the_contact(pool: PgPool) {
    let mut config = crate::config::Config::default();
    // OpenStreetMap's importer alone (the others have their own tests).
    config.catalogues.musicbrainz.enabled = false;
    config.catalogues.wikidata.enabled = false;
    config.catalogues.openalex.enabled = false;
    config.catalogues.podcastindex.enabled = false;
    config.catalogues.tranco.enabled = false;
    config.catalogues.software.enabled = false;
    let egress = crate::resolver::egress::Egress::new(config.egress.clone());
    assert!(runner::to_start(runner::importers(&config, &egress), &config).is_empty());
    runner::note_unstarted(&pool, &config).await.unwrap();
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(state.waiting.as_deref(), Some(runner::NEEDS_CONTACT));
    config.egress.contact = "ops@example.org".into();
    let started = runner::to_start(runner::importers(&config, &egress), &config);
    assert_eq!(
        started.iter().map(|i| i.catalogue()).collect::<Vec<_>>(),
        [Catalogue::Osm]
    );
    runner::note_unstarted(&pool, &config).await.unwrap();
    assert_eq!(
        store::state(&pool, Catalogue::Osm)
            .await
            .unwrap()
            .unwrap()
            .waiting,
        None
    );
    config.catalogues.osm.enabled = false;
    assert!(runner::to_start(runner::importers(&config, &egress), &config).is_empty());
}

/// A local mirror: each path's status, extra headers and body (`Content-Length` from the body
/// unless given). Counts the requests by path, but the `HEAD`s an import asks a file's version
/// with.
type Route = (&'static str, u16, Vec<(&'static str, String)>, Vec<u8>);
async fn mirror(
    routes: Vec<Route>,
) -> (
    String,
    Arc<std::sync::Mutex<std::collections::BTreeMap<String, usize>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let counted = asked.clone();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (routes, counted) = (routes.clone(), counted.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                if !request.starts_with("HEAD ") {
                    *counted.lock().unwrap().entry(path.clone()).or_default() += 1;
                }
                let route = routes.iter().find(|r| r.0 == path);
                let (status, headers, body) = match route {
                    Some((_, status, headers, body)) => (*status, headers.clone(), body.clone()),
                    None => (404, vec![], vec![]),
                };
                let mut head =
                    format!("HTTP/1.1 {status} X\r\nConnection: close\r\nETag: \"x\"\r\n");
                if !headers.iter().any(|(k, _)| *k == "Content-Length") {
                    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
                for (k, v) in headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, asked)
}
fn region_importer(cfg: &crate::config::Config, pbf_url: String, base: String) -> OsmImporter {
    OsmImporter {
        input: Input::Region,
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Import),
        layout: None,
        upstream: Some((pbf_url, base)),
        index_cap: None,
        threads: 1,
    }
}
fn with_contact(dir: &Path) -> Arc<crate::config::Config> {
    let mut cfg = (*config(dir)).clone();
    cfg.http.contact = "test@example.org".into();
    cfg.egress.contact = "test@example.org".into();
    Arc::new(cfg)
}

/// The import's download from the region's mirror: a finished file is used again by a later
/// attempt (one GET for two failed attempts); a changed region downloads its own file; a file
/// whose length leaves no room waits before any byte is written; a redirect to another host is
/// not followed.
#[sqlx::test(migrations = "../../migrations")]
async fn the_import_downloads_once_per_file_and_waits_for_room(pool: PgPool) {
    let dir = tempfile_dir();
    // A file whose header reads and whose first block does not: every attempt fails after the
    // download.
    let mut broken = region();
    let cut = broken.len() - 40;
    broken.truncate(cut);
    let (base, asked) = mirror(vec![
        ("/a.osm.pbf", 200, vec![], broken.clone()),
        ("/b.osm.pbf", 200, vec![], region()),
        (
            "/big.osm.pbf",
            200,
            vec![("Content-Length", "10000000000".into())],
            vec![],
        ),
    ])
    .await;
    let cfg = with_contact(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let a = region_importer(&cfg, format!("{base}/a.osm.pbf"), format!("{base}/updates"));
    assert!(runner::step(&a, &ctx).await.is_err());
    assert!(runner::step(&a, &ctx).await.is_err());
    assert_eq!(asked.lock().unwrap().get("/a.osm.pbf"), Some(&1));
    // Another region's file: downloaded, and the import starts over on it.
    let b = region_importer(&cfg, format!("{base}/b.osm.pbf"), format!("{base}/updates"));
    assert_eq!(runner::step(&b, &ctx).await.unwrap(), runner::Next::Again);
    assert_eq!(asked.lock().unwrap().get("/b.osm.pbf"), Some(&1));
    assert_eq!(hashes(&pool).await.len(), 6);
    // The downloaded file and its marker are gone after the import.
    assert!(!dir.join("osm/import.osm.pbf").exists() && !dir.join("osm/import.osm.done").exists());
    // No room for a 10 GB file: a wait, and nothing written.
    sqlx::query("DELETE FROM catalogue_state")
        .execute(&pool)
        .await
        .unwrap();
    let mut short = Context::new(pool.clone(), cfg.clone());
    short.free_disk = Arc::new(|_| Ok(3_000_000_000));
    let big = region_importer(
        &cfg,
        format!("{base}/big.osm.pbf"),
        format!("{base}/updates"),
    );
    assert!(matches!(
        runner::step(&big, &short).await.unwrap(),
        runner::Next::Wait(_)
    ));
    assert!(!dir.join("osm/import.part").exists());
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert!(
        state
            .waiting
            .as_deref()
            .is_some_and(|w| w.starts_with("free disk")),
        "{:?}",
        state.waiting
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Redirects are followed only to the bulk-file hosts by their exact names, over https: the
/// planet's site and the two buckets it sends files and change files to, and Geofabrik. On local
/// servers (a test client that takes their address and plain http): a chain of two redirects is
/// followed; a hop to a host not on the list is not asked at all, and neither is a local server
/// under the daemon's own policy.
#[tokio::test]
async fn redirects_go_only_to_the_bulk_file_hosts() {
    let u = |s: &str| url::Url::parse(s).unwrap();
    for ok in [
        "https://planet.openstreetmap.org/pbf/planet-261123.osm.pbf",
        "https://osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com/planet/pbf/2026/planet-261123.osm.pbf",
        "https://osm-planet-us-west-2.s3.dualstack.us-west-2.amazonaws.com/planet/replication/day/state.txt",
        "https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf",
    ] {
        assert!(crate::catalogue::fetch::may_follow(&u(ok), HOSTS), "{ok}");
    }
    for refused in [
        "http://osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com/planet/x",
        "https://osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com.evil.example/x",
        "https://osm-planet-ap-south-1.s3.dualstack.ap-south-1.amazonaws.com/planet/x",
        "https://s3.amazonaws.com/osm-planet-eu-central-1/planet/x",
        "https://evil.s3.dualstack.eu-central-1.amazonaws.com/planet/x",
        "https://planet.openstreetmap.org:8443/pbf/x",
        "https://user@download.geofabrik.de.evil.example/x",
    ] {
        assert!(
            !crate::catalogue::fetch::may_follow(&u(refused), HOSTS),
            "{refused}"
        );
    }
    let dir = tempfile_dir();
    let cfg = with_contact(&dir);
    let (c, c_asked) = mirror(vec![("/x.osm.pbf", 200, vec![], region())]).await;
    let (b, b_asked) = mirror(vec![
        (
            "/y",
            302,
            vec![("Location", format!("{c}/x.osm.pbf"))],
            vec![],
        ),
        (
            "/away",
            302,
            vec![(
                "Location",
                c.replace("127.0.0.1", "localhost") + "/x.osm.pbf",
            )],
            vec![],
        ),
    ])
    .await;
    let (a, a_asked) = mirror(vec![
        (
            "/a.osm.pbf",
            302,
            vec![("Location", format!("{b}/y"))],
            vec![],
        ),
        (
            "/b.osm.pbf",
            302,
            vec![("Location", format!("{b}/away"))],
            vec![],
        ),
    ])
    .await;
    let local = crate::catalogue::fetch::client(&cfg.egress.contact, &["127.0.0.1"]).unwrap();
    let dest = dir.join("import.osm.pbf");
    assert!(
        download(&local, &format!("{a}/a.osm.pbf"), &dest, |_| Ok(()))
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read(&dest).unwrap(), region());
    // A hop to a host not on the list: refused before it is asked.
    std::fs::remove_file(dir.join("import.osm.done")).unwrap();
    assert!(
        download(&local, &format!("{a}/b.osm.pbf"), &dest, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(c_asked.lock().unwrap().get("/x.osm.pbf"), Some(&1));
    // The daemon's client: a local server is no bulk-file host.
    assert!(download(
        &http(&cfg).unwrap(),
        &format!("{a}/a.osm.pbf"),
        &dest,
        |_| Ok(())
    )
    .await
    .is_err());
    assert_eq!(b_asked.lock().unwrap().get("/y"), Some(&1));
    assert_eq!(a_asked.lock().unwrap().get("/a.osm.pbf"), Some(&2));
    std::fs::remove_dir_all(dir).unwrap();
}

/// A bulk file is checked against the MD5 published beside the file its answer came from
/// (`<file>.md5`): one that differs, or a checksum file naming another file, is never used
/// (`checksum`, no file left), one that matches is; with none published the download goes on.
#[tokio::test]
async fn a_file_whose_published_md5_differs_is_not_used() {
    use md5::Digest as _;
    let dir = tempfile_dir();
    let cfg = with_contact(&dir);
    let body = region();
    let right: String = md5::Md5::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let wrong = "0".repeat(32);
    let line = |hex: &str, name: &str| format!("{hex}  {name}\n").into_bytes();
    let (base, asked) = mirror(vec![
        ("/bad.osm.pbf", 200, vec![], body.clone()),
        ("/bad.osm.pbf.md5", 200, vec![], line(&wrong, "bad.osm.pbf")),
        // A `-latest` name redirects to the dated file, whose checksum is the one read.
        (
            "/x-latest.osm.pbf",
            302,
            vec![("Location", "/x-261126.osm.pbf".to_owned())],
            vec![],
        ),
        (
            "/x-latest.osm.pbf.md5",
            200,
            vec![],
            line(&wrong, "x-latest.osm.pbf"),
        ),
        ("/x-261126.osm.pbf", 200, vec![], body.clone()),
        (
            "/x-261126.osm.pbf.md5",
            200,
            vec![],
            line(&right, "x-261126.osm.pbf"),
        ),
        ("/none.osm.pbf", 200, vec![], body.clone()),
        ("/other.osm.pbf", 200, vec![], body.clone()),
        (
            "/other.osm.pbf.md5",
            200,
            vec![],
            line(&wrong, "another.osm.pbf"),
        ),
    ])
    .await;
    let client = crate::catalogue::fetch::client(&cfg.egress.contact, &["127.0.0.1"]).unwrap();
    let dest = dir.join("import.osm.pbf");
    let failed = download(&client, &format!("{base}/bad.osm.pbf"), &dest, |_| Ok(()))
        .await
        .unwrap_err();
    assert_eq!(
        failed.downcast_ref::<Failure>().map(|f| f.class),
        Some("checksum")
    );
    assert!(!dest.exists() && !dir.join("import.osm.part").exists());
    assert!(!dir.join("import.osm.done").exists());
    // A checksum file that names another file: this one is not the file it lists.
    let failed = download(&client, &format!("{base}/other.osm.pbf"), &dest, |_| Ok(()))
        .await
        .unwrap_err();
    assert_eq!(
        failed.downcast_ref::<Failure>().map(|f| f.class),
        Some("checksum")
    );
    assert!(!dest.exists());
    for file in ["x-latest", "none"] {
        let _ = std::fs::remove_file(dir.join("import.osm.done"));
        let _ = std::fs::remove_file(&dest);
        assert!(
            download(
                &client,
                &format!("{base}/{file}.osm.pbf"),
                &dest,
                |_| Ok(())
            )
            .await
            .unwrap(),
            "{file}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), body, "{file}");
    }
    let asked = asked.lock().unwrap();
    assert_eq!(asked.get("/x-261126.osm.pbf.md5"), Some(&1));
    assert_eq!(asked.get("/x-latest.osm.pbf.md5"), None);
    assert_eq!(asked.get("/none.osm.pbf.md5"), Some(&1));
    drop(asked);
    std::fs::remove_dir_all(dir).unwrap();
}

/// An import whose file differs from its published MD5 fails as `checksum` and writes no card;
/// with the right file the next run imports it.
#[sqlx::test(migrations = "../../migrations")]
async fn an_import_of_a_file_that_fails_its_md5_writes_nothing(pool: PgPool) {
    use md5::Digest as _;
    let dir = tempfile_dir();
    let body = region();
    let right: String = md5::Md5::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (bad, _) = mirror(vec![
        ("/r.osm.pbf", 200, vec![], body.clone()),
        (
            "/r.osm.pbf.md5",
            200,
            vec![],
            format!("{}  r.osm.pbf\n", "f".repeat(32)).into_bytes(),
        ),
    ])
    .await;
    let (good, _) = mirror(vec![
        ("/r.osm.pbf", 200, vec![], body.clone()),
        (
            "/r.osm.pbf.md5",
            200,
            vec![],
            format!("{right} *r.osm.pbf\n").into_bytes(),
        ),
    ])
    .await;
    let cfg = with_contact(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let failing = region_importer(&cfg, format!("{bad}/r.osm.pbf"), format!("{bad}/updates"));
    let failed = runner::step(&failing, &ctx).await.unwrap_err();
    assert_eq!(
        crate::catalogue::failure::shown(&failed),
        "checksum (127.0.0.1)"
    );
    assert!(hashes(&pool).await.is_empty());
    assert!(!dir.join("osm/import.osm.pbf").exists());
    let fine = region_importer(&cfg, format!("{good}/r.osm.pbf"), format!("{good}/updates"));
    assert_eq!(
        runner::step(&fine, &ctx).await.unwrap(),
        runner::Next::Again
    );
    assert_eq!(hashes(&pool).await.len(), 6);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A cut-off run whose file changed starts over in a generation of its own, so the cards it wrote
/// from the first file go at the end: the region changed, the file lost, the file downloaded
/// anew (its earlier download expired), and a run at its end of another region.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_over_a_changed_file_leaves_none_of_the_first_files_cards(pool: PgPool) {
    let dir = tempfile_dir();
    let (base, _) = mirror(vec![
        ("/a.osm.pbf", 200, vec![], region()),
        ("/b.osm.pbf", 200, vec![], region()),
    ])
    .await;
    let cfg = with_contact(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let (a_url, b_url) = (format!("{base}/a.osm.pbf"), format!("{base}/b.osm.pbf"));
    let file = dir.join("osm/import.osm.pbf");
    let stray = place::read(
        "node",
        777,
        &place::tags([("amenity", "atm")]),
        place::Geometry {
            point: Some(Point {
                lat: 47.0,
                lon: 9.0,
            }),
            bbox: None,
        },
    )
    .unwrap();
    for case in [
        "region changed",
        "file lost",
        "downloaded anew",
        "done, of another region",
    ] {
        sqlx::query("DELETE FROM catalogue_card")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM catalogue_state")
            .execute(&pool)
            .await
            .unwrap();
        store::ensure_state(&pool, Catalogue::Osm).await.unwrap();
        std::fs::create_dir_all(dir.join("osm")).unwrap();
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(dir.join("osm/import.osm.done"));
        let (url, stage) = match case {
            "region changed" => (&a_url, "pass1"),
            "done, of another region" => (&a_url, "done"),
            _ => (&b_url, "pass1"),
        };
        if case != "file lost" {
            std::fs::write(&file, region()).unwrap();
            std::fs::write(dir.join("osm/import.osm.done"), url).unwrap();
        }
        if case == "downloaded anew" {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86400);
            std::fs::File::options()
                .write(true)
                .open(dir.join("osm/import.osm.done"))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let cursor = json!({"stage":stage,"blob":0,"offset":0,"index":0,"pending":0,"ways_at":null,
            "file":file.display().to_string(),"downloaded":true,"url":url,"layout":"sparse",
            "header":{"sequence":null,"timestamp":"2025-11-24T16:00:00Z","name":"import.osm.pbf"}});
        sqlx::query("UPDATE catalogue_state SET run='import',phase='import',generation=5,cursor=$1 WHERE catalogue='osm'")
            .bind(&cursor)
            .execute(&pool)
            .await
            .unwrap();
        // What the cut-off run wrote from its file.
        store::upsert(&pool, Catalogue::Osm, 5, std::slice::from_ref(&stray), None)
            .await
            .unwrap();
        let b = region_importer(&cfg, b_url.clone(), format!("{base}/updates"));
        assert_eq!(
            runner::step(&b, &ctx).await.unwrap(),
            runner::Next::Again,
            "{case}"
        );
        let cards = hashes(&pool).await;
        assert!(
            !cards.contains_key("node/777"),
            "{case}: {:?}",
            cards.keys()
        );
        assert_eq!(cards.len(), 6, "{case}");
        let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
        assert!(state.generation > 5 && state.run.is_none(), "{case}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A dense index that takes more disk than its room stops the first pass (`disk`).
#[sqlx::test(migrations = "../../migrations")]
async fn an_index_past_its_room_stops_the_import(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    let cfg = config(&dir);
    let importer = OsmImporter {
        input: Input::Files(vec![pbf]),
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Import),
        layout: Some(nodes::Layout::Dense),
        upstream: None,
        index_cap: Some(1),
        threads: 1,
    };
    let mut ctx = Context::new(pool.clone(), cfg);
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let failed = runner::step(&importer, &ctx).await.unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("disk"),
        "{failed:#}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A run cut off after its last part (in the runner's sweep) finishes without its file.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_cut_off_at_its_end_finishes_without_the_file(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf.clone()], false).await;
    let generation: i64 =
        sqlx::query_scalar("SELECT generation FROM catalogue_state WHERE catalogue='osm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let cursor = json!({"stage":"done","file":pbf.display().to_string(),"downloaded":false,
        "header":{"sequence":4978,"timestamp":"2025-11-24T16:00:00Z","name":"region.osm.pbf"}});
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',cursor=$1,position=NULL")
        .bind(&cursor)
        .execute(&pool)
        .await
        .unwrap();
    std::fs::remove_file(&pbf).unwrap();
    assert_eq!(
        step(&pool, &dir, Run::Import, vec![pbf], false).await,
        runner::Next::Again
    );
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(
        (state.run, state.generation, state.cards),
        (None, generation, 6)
    );
    assert_eq!(state.position.unwrap()["sequence"], 4978);
    std::fs::remove_dir_all(dir).unwrap();
}

/// `--from` another file while an import of one is under way is refused.
#[sqlx::test(migrations = "../../migrations")]
async fn another_file_while_an_import_is_under_way_is_refused(pool: PgPool) {
    let dir = tempfile_dir();
    let (a, b) = (dir.join("a.osm.pbf"), dir.join("b.osm.pbf"));
    std::fs::write(&a, region()).unwrap();
    std::fs::write(&b, region()).unwrap();
    store::ensure_state(&pool, Catalogue::Osm).await.unwrap();
    let cursor = json!({"stage":"pass1","blob":1,"offset":0,"index":0,"pending":0,"ways_at":null,"file":a.display().to_string(),"layout":"sparse"});
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',generation=1,cursor=$1")
        .bind(&cursor)
        .execute(&pool)
        .await
        .unwrap();
    let cfg = config(&dir);
    let importer = OsmImporter {
        input: Input::Files(vec![b]),
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Import),
        layout: None,
        upstream: None,
        index_cap: None,
        threads: 1,
    };
    let mut ctx = Context::new(pool.clone(), cfg);
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let failed = runner::step(&importer, &ctx).await.unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("other_file"),
        "{failed:#}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Versions out of file order: the newest wins whatever its place, and a delete wins a tie.
#[sqlx::test(migrations = "../../migrations")]
async fn the_newest_version_wins_in_any_order(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let edits = r#"<osmChange version="0.6">
  <delete><node id="9" version="3"/></delete>
  <modify><node id="9" version="2" lat="47.14" lon="9.54"><tag k="amenity" v="restaurant"/><tag k="name" v="Löwen Neu"/></node></modify>
  <delete><node id="8" version="3"/></delete>
  <modify><node id="8" version="3" lat="47.13" lon="9.53"><tag k="amenity" v="restaurant"/><tag k="name" v="Rössli"/></node></modify>
  <modify><node id="1" version="2" lat="47.141" lon="9.5215"><tag k="amenity" v="cafe"/><tag k="name" v="Engel Alt"/></node></modify>
  <modify><node id="1" version="3" lat="47.141" lon="9.5215"><tag k="amenity" v="cafe"/><tag k="name" v="Engel Neu"/></node></modify>
</osmChange>"#;
    let changes = dir.join("3.osc.gz");
    std::fs::write(&changes, gz(edits)).unwrap();
    step(&pool, &dir, Run::Update, vec![changes], false).await;
    assert_eq!(name(&pool, "node/9").await, None);
    assert_eq!(name(&pool, "node/8").await, None);
    // The newest last: it wins too.
    assert_eq!(name(&pool, "node/1").await.as_deref(), Some("Engel Neu"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_way_across_the_antimeridian_has_its_point_on_it() {
    let at = |lat: f64, lon: f64| ((lat * 1e7) as i32, (lon * 1e7) as i32);
    let g = way_geometry(&[
        at(-17.0, 179.9),
        at(-17.0, -179.9),
        at(-17.1, -179.9),
        at(-17.0, 179.9),
    ]);
    let p = g.point.unwrap();
    assert!(p.lon.abs() > 179.8, "{p:?}");
    let b = g.bbox.unwrap();
    assert!(
        b[0] > b[2] && (b[0] - 179.9).abs() < 1e-6 && (b[2] + 179.9).abs() < 1e-6,
        "{b:?}"
    );
    assert!((centre(b).lon.abs() - 180.0).abs() < 1e-6);
    // A relation of ways either side of it: its box across it, its centre on it.
    let west = way_geometry(&[at(-17.0, 179.9), at(-17.1, 180.0)])
        .bbox
        .unwrap();
    let east = way_geometry(&[at(-17.0, -180.0), at(-17.1, -179.8)])
        .bbox
        .unwrap();
    let both = union(Some(west), east);
    assert!(
        (both[0] - 179.9).abs() < 1e-6 && (both[2] + 179.8).abs() < 1e-6,
        "{both:?}"
    );
    assert!(
        (centre(both).lon.abs() - 179.95).abs() < 1e-6,
        "{:?}",
        centre(both)
    );
    assert_eq!(union(Some(east), west), both);
    // Ordinary boxes join as before; a box round the globe stays one.
    assert_eq!(
        union(Some([9.0, 47.0, 9.1, 47.1]), [9.2, 46.9, 9.3, 47.0]),
        [9.0, 46.9, 9.3, 47.1]
    );
    assert_eq!(
        union(Some([-170.0, 0.0, -10.0, 1.0]), [10.0, 0.0, 170.0, 1.0])[0],
        -170.0
    );
    // An ordinary way keeps its plain box.
    let g = way_geometry(&[at(47.0, 9.0), at(47.1, 9.1)]);
    assert_eq!(g.bbox.unwrap(), [9.0, 47.0, 9.1, 47.1]);
}

/// A header naming another replication stream (the planet's minutely one) gives no sequence: the
/// updates start from the file's time.
#[sqlx::test(migrations = "../../migrations")]
async fn a_header_of_another_stream_gives_no_sequence(pool: PgPool) {
    let dir = tempfile_dir();
    let header = pbf::Header {
        required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
        optional_features: vec!["Sort.Type_then_ID".into()],
        replication_timestamp: Some(1_764_000_000),
        replication_sequence: Some(7_387_014),
        replication_base_url: Some("https://planet.openstreetmap.org/replication/minute".into()),
    };
    let pbf = dir.join("planet-part.osm.pbf");
    std::fs::write(
        &pbf,
        file(
            &header,
            &[data_block(&[Node(1, 47.1, 9.5, &[("amenity", "atm")])])],
        ),
    )
    .unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(state.position.unwrap()["sequence"], Value::Null);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Benchmark, not a gate (ignored): the node index of a real extract in both layouts.
/// `PINAKES_OSM_BENCH_PBF=<extract> PINAKES_OSM_BENCH_DIR=<dir> cargo test -p pinakes-core --lib
/// osm::tests::node_index_layouts_on_a_real_extract -- --ignored --nocapture`.
///
/// The dense layout takes 8 bytes per ID up to the highest (about 100 GB at today's IDs, for any
/// extract), so here the extract's node IDs are renumbered 1..n in order: the file then holds
/// 8 bytes per node, and each lookup is the same one 8-byte read at `id × 8` it is on the planet.
/// The sparse layout runs on the real IDs. Each writes every node and looks up every node of the
/// ways the filter keeps, as the first pass does; then the dense lookups again with the file
/// dropped from the page cache (`POSIX_FADV_DONTNEED`), each 4 KiB page read from disk once.
/// Prints one JSON line.
#[test]
#[ignore]
fn node_index_layouts_on_a_real_extract() {
    use std::os::fd::AsRawFd;
    use std::time::Instant;
    let src = PathBuf::from(std::env::var("PINAKES_OSM_BENCH_PBF").expect("PINAKES_OSM_BENCH_PBF"));
    let dir = PathBuf::from(std::env::var("PINAKES_OSM_BENCH_DIR").expect("PINAKES_OSM_BENCH_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let blocks = || {
        let mut reader = pbf::Reader::new(
            std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(&src).unwrap()),
            0,
        );
        std::iter::from_fn(move || loop {
            let blob = reader.next_blob().unwrap()?;
            if blob.kind == "OSMData" {
                return Some(pbf::block(&pbf::inflate(&blob.data).unwrap()).unwrap());
            }
        })
    };
    // Pass 1: every node into both indexes (the dense one by rank).
    let (sparse_path, dense_path) = (dir.join("sparse.idx"), dir.join("dense.idx"));
    let mut sparse = nodes::Writer::open(&sparse_path, nodes::Layout::Sparse, 0).unwrap();
    let mut dense = nodes::Writer::open(&dense_path, nodes::Layout::Dense, 0).unwrap();
    let mut ids: Vec<i64> = vec![];
    let (mut write_sparse, mut write_dense) = (0f64, 0f64);
    let started = Instant::now();
    let mut kept_ways: Vec<Vec<i64>> = vec![];
    for b in blocks() {
        if !b.nodes.is_empty() {
            let t = Instant::now();
            for n in &b.nodes {
                sparse.add(n.id, n.lat, n.lon).unwrap();
            }
            write_sparse += t.elapsed().as_secs_f64();
            let t = Instant::now();
            for n in &b.nodes {
                ids.push(n.id);
                dense.add(ids.len() as i64, n.lat, n.lon).unwrap();
            }
            write_dense += t.elapsed().as_secs_f64();
        }
        for w in &b.ways {
            let tags = place::tags(b.tags(&w.tags));
            if place::kept(&tags).is_some() {
                kept_ways.push(w.refs.clone());
            }
        }
    }
    let t = Instant::now();
    sparse.sync().unwrap();
    write_sparse += t.elapsed().as_secs_f64();
    let t = Instant::now();
    dense.sync().unwrap();
    write_dense += t.elapsed().as_secs_f64();
    let read_s = started.elapsed().as_secs_f64() - write_sparse - write_dense;
    let refs: usize = kept_ways.iter().map(Vec::len).sum();
    // The dense IDs of each way's nodes, worked out before the clock starts.
    let ranked: Vec<Vec<i64>> = kept_ways
        .iter()
        .map(|w| {
            w.iter()
                .map(|id| ids.binary_search(id).map_or(0, |i| i as i64 + 1))
                .collect()
        })
        .collect();
    let lookups = |path: &Path, layout: nodes::Layout, ways: &[Vec<i64>]| {
        let r = nodes::Reader::open(path, layout).unwrap();
        let t = Instant::now();
        let mut found = 0u64;
        for w in ways {
            for id in w {
                found += u64::from(r.get(*id).unwrap().is_some());
            }
        }
        (t.elapsed().as_secs_f64(), r.reads(), found)
    };
    let (sparse_s, sparse_reads, sparse_found) =
        lookups(&sparse_path, nodes::Layout::Sparse, &kept_ways);
    let (dense_s, dense_reads, dense_found) = lookups(&dense_path, nodes::Layout::Dense, &ranked);
    // Cold: the dense file out of the page cache first.
    let drop_cache = |path: &Path| {
        let f = std::fs::File::open(path).unwrap();
        f.sync_all().unwrap();
        // SAFETY: a valid open descriptor; advice only.
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    };
    drop_cache(&dense_path);
    let (cold_s, _, _) = lookups(&dense_path, nodes::Layout::Dense, &ranked);
    // Pages a lookup lands on that differ from the previous lookup's (the reads a cold cache
    // makes at most, when no page is read twice).
    let mut changes = 0u64;
    let mut last = u64::MAX;
    for w in &ranked {
        for id in w {
            let page = (*id as u64 * 8) / 4096;
            changes += u64::from(page != last);
            last = page;
        }
    }
    let report = json!({
        "file": src.file_name().map(|n| n.to_string_lossy().into_owned()),
        "nodes": ids.len(), "kept_ways": kept_ways.len(), "kept_way_refs": refs,
        "decode_s": read_s,
        "sparse": {"bytes": std::fs::metadata(&sparse_path).unwrap().len(), "write_s": write_sparse, "lookup_s": sparse_s, "reads": sparse_reads, "found": sparse_found, "us_per_lookup": sparse_s * 1e6 / refs as f64},
        "dense": {"bytes": std::fs::metadata(&dense_path).unwrap().len(), "write_s": write_dense, "lookup_s": dense_s, "reads": dense_reads, "found": dense_found, "us_per_lookup": dense_s * 1e6 / refs as f64,
                  "cold_lookup_s": cold_s, "cold_us_per_lookup": cold_s * 1e6 / refs as f64, "page_changes": changes},
    });
    println!("{report}");
    std::fs::remove_file(sparse_path).unwrap();
    std::fs::remove_file(dense_path).unwrap();
}

/// A place renamed a little (no unrelated rename) and then deleted keeps its last name for the
/// deleted-place rule, whatever a read of its link stores later; back again, it is no removed
/// object.
#[sqlx::test(migrations = "../../migrations")]
async fn a_deleted_place_keeps_its_last_name(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let renamed = dir.join("4.osc.gz");
    std::fs::write(&renamed, gz(r#"<osmChange version="0.6"><modify><node id="9" version="2" lat="47.14" lon="9.54"><tag k="amenity" v="restaurant"/><tag k="name" v="Löwen Vaduz"/></node></modify></osmChange>"#)).unwrap();
    step(&pool, &dir, Run::Update, vec![renamed], false).await;
    let renames: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_rename")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(renames, 0);
    let deleted = dir.join("5.osc.gz");
    std::fs::write(
        &deleted,
        gz(r#"<osmChange version="0.6"><delete><node id="9" version="3"/></delete></osmChange>"#),
    )
    .unwrap();
    step(&pool, &dir, Run::Update, vec![deleted], false).await;
    assert_eq!(name(&pool, "node/9").await, None);
    // A read of the link stores the object as missing, without a name.
    sqlx::query("INSERT INTO osm_place(osm_type,osm_id,type,type_path,doc,status,fetched_at,refresh_at,fetch_doc) VALUES('node',9,'Place',ARRAY['Thing','Place'],'{}','missing',now(),now()+interval '1 day','{}')")
        .execute(&pool)
        .await
        .unwrap();
    let (last, category, lat, _) = rules::last_place(&pool, "node", 9).await.unwrap().unwrap();
    assert_eq!(
        (last.as_str(), category.as_str()),
        ("Löwen Vaduz", "amenity/restaurant")
    );
    assert!((lat - 47.14).abs() < 1e-6);
    let back = dir.join("6.osc.gz");
    std::fs::write(&back, gz(r#"<osmChange version="0.6"><create><node id="9" version="4" lat="47.14" lon="9.54"><tag k="amenity" v="restaurant"/><tag k="name" v="Löwen"/></node></create></osmChange>"#)).unwrap();
    step(&pool, &dir, Run::Update, vec![back], false).await;
    let removed: i64 = sqlx::query_scalar("SELECT count(*) FROM osm_removed")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(removed, 0);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A re-import marks due the shared links of the cards it changes, not of those it reads the
/// same.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reimport_marks_only_changed_cards_links_due(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf.clone()], false).await;
    let mut links = vec![];
    for id in [9, 1] {
        let uri = format!("https://www.openstreetmap.org/node/{id}");
        links.push(crate::store::ensure_resource(&pool, &uri).await.unwrap());
    }
    sqlx::query("UPDATE resource SET next_attempt_at=now()+interval '30 days',priority_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    // Node 9's stored card differs from what the file gives: the re-import rewrites it.
    sqlx::query("UPDATE catalogue_card SET content_hash=decode(repeat('00',32),'hex') WHERE source_id='node/9'")
        .execute(&pool)
        .await
        .unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let due: Vec<(String, bool)> =
        sqlx::query_as("SELECT id,priority_at IS NOT NULL FROM resource WHERE id=ANY($1)")
            .bind(&links)
            .fetch_all(&pool)
            .await
            .unwrap();
    let due: std::collections::HashMap<String, bool> = due.into_iter().collect();
    assert_eq!((due[&links[0]], due[&links[1]]), (true, false));
    std::fs::remove_dir_all(dir).unwrap();
}

fn files_importer(cfg: &crate::config::Config, files: Vec<PathBuf>) -> OsmImporter {
    OsmImporter {
        input: Input::Files(files),
        region: cfg.catalogues.osm.region.clone(),
        force: Some(Run::Import),
        layout: None,
        upstream: None,
        index_cap: None,
        threads: 1,
    }
}
/// A file of one place (an extract cut off at its start).
fn one_place() -> Vec<u8> {
    let header = pbf::Header {
        required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
        optional_features: vec!["Sort.Type_then_ID".into()],
        replication_timestamp: Some(1_764_000_000),
        replication_sequence: Some(4978),
        replication_base_url: Some(
            "https://download.geofabrik.de/europe/liechtenstein/updates".into(),
        ),
    };
    file(
        &header,
        &[data_block(&[Node(
            1,
            47.141,
            9.5215,
            &[("amenity", "cafe"), ("name", "Engel")],
        )])],
    )
}

/// A re-import that would sweep most of the catalogue (a cut-off file) removes nothing and fails
/// `shrunk`, as any catalogue's full import does; once the operator allows it, the same run
/// goes through and removes the rest.
#[sqlx::test(migrations = "../../migrations")]
async fn a_reimport_far_smaller_waits_for_the_allowance(pool: PgPool) {
    let dir = tempfile_dir();
    let (full, small) = (dir.join("region.osm.pbf"), dir.join("small.osm.pbf"));
    std::fs::write(&full, region()).unwrap();
    std::fs::write(&small, one_place()).unwrap();
    step(&pool, &dir, Run::Import, vec![full], false).await;
    assert_eq!(hashes(&pool).await.len(), 6);
    let cfg = config(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let importer = files_importer(&cfg, vec![small]);
    let failed = runner::step(&importer, &ctx).await.unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("shrunk"),
        "{failed:#}"
    );
    assert_eq!(hashes(&pool).await.len(), 6);
    // Waiting for the allowance, it holds no index.
    assert!(!dir.join("osm/nodes.idx").exists() && !dir.join("osm/relations.jsonl").exists());
    // The run loop records the failure; then the operator allows it.
    runner::record_failure(&pool, Catalogue::Osm, "shrunk")
        .await
        .unwrap();
    assert_eq!(
        store::allow_shrink(&pool, Catalogue::Osm).await.unwrap(),
        store::Allowance::Allowed
    );
    assert_eq!(
        runner::step(&importer, &ctx).await.unwrap(),
        runner::Next::Again
    );
    assert_eq!(hashes(&pool).await.keys().collect::<Vec<_>>(), ["node/1"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A file that holds no place is refused (`empty`) and is not read again (asked again, the same
/// file ends the run at once); the next attempt on another file starts over.
#[sqlx::test(migrations = "../../migrations")]
async fn an_empty_file_does_not_hold_up_the_next_one(pool: PgPool) {
    let dir = tempfile_dir();
    let header = pbf::Header {
        required_features: vec!["OsmSchema-V0.6".into(), "DenseNodes".into()],
        ..Default::default()
    };
    let (empty, full) = (dir.join("empty.osm.pbf"), dir.join("region.osm.pbf"));
    std::fs::write(
        &empty,
        file(&header, &[data_block(&[Node(1, 47.1, 9.5, &[])])]),
    )
    .unwrap();
    std::fs::write(&full, region()).unwrap();
    let cfg = config(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let failed = runner::step(&files_importer(&cfg, vec![empty.clone()]), &ctx)
        .await
        .unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("empty (no record read in "),
        "{failed:#}"
    );
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert!(state.cursor.is_none(), "{:?}", state.cursor);
    assert!(state.last_import_at.is_none());
    let position = state.position.unwrap();
    assert_eq!(
        position["refused_source"],
        json!(empty.display().to_string())
    );
    let again = runner::step(&files_importer(&cfg, vec![empty]), &ctx)
        .await
        .unwrap_err();
    assert!(again.is::<runner::StillRefused>(), "{again:#}");
    assert_eq!(
        runner::step(&files_importer(&cfg, vec![full]), &ctx)
            .await
            .unwrap(),
        runner::Next::Again
    );
    assert_eq!(hashes(&pool).await.len(), 6);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A way newly kept by an update with only some of its nodes in the file has no geometry (not
/// one from the nodes the file holds), until the next full import.
#[sqlx::test(migrations = "../../migrations")]
async fn a_way_newly_kept_without_its_nodes_has_no_geometry(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let edits = r#"<osmChange version="0.6"><create><node id="900" version="1" lat="47.10" lon="9.50"/></create>
<modify><way id="901" version="2"><nd ref="900"/><nd ref="9999"/><nd ref="9998"/><tag k="amenity" v="parking"/><tag k="name" v="P Rathaus"/></way></modify></osmChange>"#;
    let changes = dir.join("7.osc.gz");
    std::fs::write(&changes, gz(edits)).unwrap();
    step(&pool, &dir, Run::Update, vec![changes], false).await;
    let (point, bbox): (bool, bool) = sqlx::query_as(
        "SELECT point IS NOT NULL,details ? 'bbox' FROM catalogue_card WHERE source_id='way/901'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((point, bbox), (false, false));
    std::fs::remove_dir_all(dir).unwrap();
}

/// The region imported, then a shared place the file does not hold (node 777, as if deleted
/// since), and one it holds listed as removed (node 9). The file's path.
async fn a_shared_place_gone(pool: &PgPool, dir: &Path) -> PathBuf {
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(pool, dir, Run::Import, vec![pbf.clone()], false).await;
    let gone = place::read(
        "node",
        777,
        &place::tags([("amenity", "restaurant"), ("name", "Sonne")]),
        place::Geometry {
            point: Some(Point {
                lat: 47.15,
                lon: 9.52,
            }),
            bbox: None,
        },
    )
    .unwrap();
    let generation: i64 =
        sqlx::query_scalar("SELECT generation FROM catalogue_state WHERE catalogue='osm'")
            .fetch_one(pool)
            .await
            .unwrap();
    store::upsert(pool, Catalogue::Osm, generation, &[gone], None)
        .await
        .unwrap();
    crate::store::ensure_resource(pool, "https://www.openstreetmap.org/node/777")
        .await
        .unwrap();
    sqlx::query("INSERT INTO osm_removed(osm_type,osm_id,name,category,point) VALUES('node',9,'Löwen','amenity/restaurant',ST_SetSRID(ST_MakePoint(9.54,47.14),4326))")
        .execute(pool)
        .await
        .unwrap();
    pbf
}
async fn removed(pool: &PgPool) -> Vec<(String, i64, String)> {
    sqlx::query_as("SELECT osm_type,osm_id,name FROM osm_removed ORDER BY osm_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A named place a full import sweeps, whose link someone shared, keeps its last card in
/// `osm_removed`; read again by a later full import, it is no removed object.
#[sqlx::test(migrations = "../../migrations")]
async fn a_swept_shared_place_is_removed_until_a_file_holds_it_again(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = a_shared_place_gone(&pool, &dir).await;
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    assert_eq!(
        removed(&pool).await,
        [("node".to_owned(), 777, "Sonne".to_owned())]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A full import of another region sweeps the last region's places, shared ones too, but records
/// none as removed (nor does the ODbL dump list one): they still exist in OpenStreetMap.
#[sqlx::test(migrations = "../../migrations")]
async fn another_regions_import_records_no_removed_place(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = a_shared_place_gone(&pool, &dir).await;
    let mut cfg = (*config(&dir)).clone();
    cfg.catalogues.osm.region = "europe/switzerland".into();
    let importer = files_importer(&cfg, vec![pbf]);
    let mut ctx = Context::new(pool.clone(), Arc::new(cfg));
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    runner::step(&importer, &ctx).await.unwrap();
    assert_eq!(name(&pool, "node/777").await, None);
    assert_eq!(removed(&pool).await, []);
    let lines: Vec<Value> = sqlx::query_scalar(crate::api::OSM_DUMP_REMOVED_LINE)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(lines, Vec::<Value>::new());
    let scope: Value =
        sqlx::query_scalar("SELECT scope FROM catalogue_state WHERE catalogue='osm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(scope["region"], "europe/switzerland");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A change file whose removals would pass a tenth of the catalogue's cards (and ten) is refused
/// before anything of it is applied: no card goes, the position names it, and the same file is
/// not read again (asked again, the run ends at once).
#[sqlx::test(migrations = "../../migrations")]
async fn a_change_file_that_removes_too_much_is_refused(pool: PgPool) {
    let dir = tempfile_dir();
    let pbf = dir.join("region.osm.pbf");
    std::fs::write(&pbf, region()).unwrap();
    step(&pool, &dir, Run::Import, vec![pbf], false).await;
    let node = |id: u32, action: &str| {
        match action {
        "create" => format!("<node id=\"{id}\" version=\"1\" changeset=\"200\" timestamp=\"2026-11-25T10:00:00Z\" lat=\"47.1{id}\" lon=\"9.5{id}\"><tag k=\"amenity\" v=\"restaurant\"/><tag k=\"name\" v=\"Place {id}\"/></node>"),
        _ => format!("<node id=\"{id}\" version=\"2\" changeset=\"201\" timestamp=\"2026-11-26T10:00:00Z\"/>"),
    }
    };
    let file = |action: &str, ids: std::ops::Range<u32>| {
        let nodes: String = ids.map(|id| node(id, action)).collect();
        format!("<?xml version='1.0' encoding='UTF-8'?><osmChange version=\"0.6\"><{action}>{nodes}</{action}></osmChange>")
    };
    let created = dir.join("created.osc.gz");
    std::fs::write(&created, gz(&file("create", 100..120))).unwrap();
    step(&pool, &dir, Run::Update, vec![created], false).await;
    let before = hashes(&pool).await;
    assert_eq!(before.len(), 26);
    // Twelve of 26 deleted: refused.
    let deleted = dir.join("deleted.osc.gz");
    std::fs::write(&deleted, gz(&file("delete", 100..112))).unwrap();
    let cfg = config(&dir);
    let mut ctx = Context::new(pool.clone(), cfg.clone());
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    let mut importer = files_importer(&cfg, vec![deleted.clone()]);
    importer.force = Some(Run::Update);
    let failed = runner::step(&importer, &ctx).await.unwrap_err();
    assert!(
        crate::catalogue::failure::shown(&failed).starts_with("refused (change file"),
        "{failed:#}"
    );
    assert_eq!(hashes(&pool).await, before);
    let state = store::state(&pool, Catalogue::Osm).await.unwrap().unwrap();
    assert_eq!(
        state.position.unwrap()["refused"],
        json!(deleted.display().to_string())
    );
    let again = runner::step(&importer, &ctx).await.unwrap_err();
    assert!(again.is::<runner::StillRefused>(), "{again:#}");
    // Ten deleted (the floor): applied.
    let ten = dir.join("ten.osc.gz");
    std::fs::write(&ten, gz(&file("delete", 100..110))).unwrap();
    step(&pool, &dir, Run::Update, vec![ten], false).await;
    assert_eq!(hashes(&pool).await.len(), 16);
    std::fs::remove_dir_all(dir).unwrap();
}
