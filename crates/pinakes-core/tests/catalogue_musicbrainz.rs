//! The MusicBrainz importer over dumps packed from real dump lines
//! (`fixtures/catalogue/musicbrainz`): the cards it makes, a new dump's update (only what
//! changed is written), the ISRC rule, `limit`, the checksum and removal guards, a run that
//! resumes, joins of shared links and search with the catalogue on and off.
use pinakes_core::{
    api::{self, AppState},
    catalogue::{
        join, link_key,
        musicbrainz::{MusicBrainz, Source},
        run::{self, Context, Pace, Run},
        store, Catalogue,
    },
    config::{Config, QueueConfig},
};
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::PgPool;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod common;

const A: &str = "20261121-001001";
const B: &str = "20261125-001001";
const TIMESTAMP: &str = "2026-11-25 01:17:36.116289+00\n";
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/catalogue/musicbrainz"
);
/// Tori Amos' "A Sorta Fairytale (the 101 mix)", on both editions of the single.
const MIX: &str = "aeeaa6df-4ab1-4eee-b72f-f21b5c7cbdfe";
/// "Operation Peter Pan", with Spotify, Deezer, Apple Music and TIDAL links.
const PETER_PAN: &str = "551938f5-faed-4ad0-bf20-07a3a6b7298d";
const PETER_PAN_SPOTIFY: &str = "https://open.spotify.com/track/78yBne888ORCHuPLngszlS";
/// "A Sorta Fairytale (original single version)": no genres of its own.
const SINGLE: &str = "2b4db5d9-4b60-4f12-a94b-717877ea2692";
/// Arovane's "Tabl F", ISRC USHMD1320901.
const TABL_F: &str = "786a680c-c43b-4a68-8885-152a0d5a64d2";
const AMC: &str = "5837d14e-4d18-49cf-a759-e5818714bce4";
const ROSEMARY: &str = "41418807-a068-3cea-ae20-bf888ebf71e7";
const TRITONIS: &str = "248f098b-863c-38f6-b703-4db66727ed9c";
const ARTIFACTS: &str = "a2b51cf6-2430-4edc-a3ce-3d234c514566";

fn lines(entity: &str) -> Vec<Value> {
    std::fs::read_to_string(format!("{FIXTURES}/{entity}.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
/// A ustar archive of these files.
fn tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = vec![];
    for (path, data) in files {
        let mut h = [0u8; 512];
        h[..path.len()].copy_from_slice(path.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        h[156] = b'0';
        h[257..265].copy_from_slice(b"ustar  \0");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}
fn xz(data: &[u8]) -> Vec<u8> {
    let mut encoder = liblzma::write::XzEncoder::new(vec![], 3);
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}
/// A dump `<root>/<version>/` of the fixture lines, as `edit` changes them, with its
/// `SHA256SUMS`.
fn dump(root: &Path, version: &str, edit: impl Fn(&str, &mut Vec<Value>)) -> PathBuf {
    let dir = root.join(version);
    std::fs::create_dir_all(&dir).unwrap();
    let mut sums = String::new();
    for entity in ["artist", "release-group", "release", "recording"] {
        let mut entities = lines(entity);
        edit(entity, &mut entities);
        let body: String = entities.iter().map(|e| format!("{e}\n")).collect();
        let packed = xz(&tar(&[
            ("TIMESTAMP", TIMESTAMP.as_bytes()),
            ("README", b"one entity per line\n"),
            (&format!("mbdump/{entity}"), body.as_bytes()),
        ]));
        let name = format!("{entity}.tar.xz");
        let digest: String = sha2::Sha256::digest(&packed)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        sums.push_str(&format!("{digest} *{name}\n"));
        std::fs::write(dir.join(name), packed).unwrap();
    }
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    dir
}
fn unchanged(_: &str, _: &mut Vec<Value>) {}

struct Setup {
    ctx: Context,
    root: tempfile::TempDir,
}
fn setup(pool: &PgPool, limit: Option<u64>) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.catalogues.dir = root
        .path()
        .join("catalogues")
        .to_string_lossy()
        .into_owned();
    config.catalogues.musicbrainz.limit = limit;
    let mut ctx = Context::new(pool.clone(), Arc::new(config));
    ctx.free_disk = Arc::new(|_| Ok(u64::MAX));
    ctx.pace = Pace {
        retry: Duration::from_millis(20),
        disk: Duration::from_millis(20),
        turn: Duration::from_millis(20),
        lock_check: Duration::from_millis(500),
    };
    Setup { ctx, root }
}
async fn run(s: &Setup, dir: &Path, kind: Run) -> anyhow::Result<store::State> {
    let importer = MusicBrainz::new(Source::Dir(dir.to_owned()), "")
        .unwrap()
        .threads(2)
        .limit(s.ctx.config.catalogues.musicbrainz.limit);
    run::run_once(Arc::new(importer), &s.ctx, kind, |_| {}).await
}
#[derive(sqlx::FromRow, Debug, Clone, PartialEq)]
struct Row {
    source_id: String,
    uri: String,
    r#type: String,
    name: String,
    other_names: Vec<String>,
    description: Option<String>,
    labels: Vec<String>,
    keys: Vec<String>,
    related_uris: Vec<String>,
    fame: f32,
    details: Value,
    version: String,
}
async fn rows(pool: &PgPool) -> Vec<Row> {
    sqlx::query_as("SELECT source_id,uri,type,name,other_names,description,labels,keys,related_uris,fame,details,xmin::text AS version FROM catalogue_card WHERE catalogue='musicbrainz' ORDER BY source_id COLLATE \"C\"")
        .fetch_all(pool)
        .await
        .unwrap()
}
fn row<'a>(rows: &'a [Row], source_id: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.source_id == source_id)
        .unwrap_or_else(|| panic!("no card {source_id}"))
}
fn fame(releases: f64) -> f32 {
    ((1.0 + releases).ln() / 1001f64.ln()) as f32
}
fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// The dump's artists, release groups and recordings become cards: canonical links, types,
/// names, descriptions, genres as labels, keys (MBIDs, unique ISRCs, Wikidata, the music-service
/// pages MusicBrainz relates), official websites as related links, fame from releases, and the
/// state the run leaves.
#[sqlx::test(migrations = "../../migrations")]
async fn a_dump_becomes_cards(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    let state = run(&s, &dir, Run::Import).await.unwrap();
    let rows = rows(&pool).await;
    // 2 artists, 2 release groups, 3 + 4 recordings on the releases and 2 on none. The releases'
    // own artists and release groups are not in the fixture's artist and release group files.
    assert_eq!(
        rows.len(),
        13,
        "{:#?}",
        rows.iter().map(|r| &r.source_id).collect::<Vec<_>>()
    );
    assert_eq!(state.cards, 13);
    assert_eq!(state.upstream_version.as_deref(), Some(B));
    assert_eq!(state.position, Some(json!({ "version": B })));
    assert_eq!(
        state.current_as_of.unwrap().to_rfc3339(),
        "2026-11-25T01:17:36.116289+00:00"
    );
    assert!(state.last_import_at.is_some() && state.run.is_none());

    // A recording on both editions of a single.
    let mix = row(&rows, &format!("recording/{MIX}"));
    assert_eq!(mix.uri, format!("https://musicbrainz.org/recording/{MIX}"));
    assert_eq!(mix.r#type, "MusicRecording");
    assert_eq!(mix.name, "A Sorta Fairytale (the 101 mix)");
    assert_eq!(
        mix.description.as_deref(),
        Some("Tori Amos · A Sorta Fairytale (2001) · 4:04")
    );
    assert_eq!(
        sorted(mix.labels.clone()),
        ["alternative-rock", "punk", "rock"]
    );
    assert_eq!(mix.fame, fame(2.0));
    assert_eq!(
        mix.keys[..3],
        [
            link_key(&mix.uri).unwrap(),
            format!("mb:recording:{MIX}"),
            "isrc:USSM10211499".to_owned()
        ]
    );
    assert_eq!(
        mix.details["release_group"],
        "51e8835b-98de-4cf9-aae1-6315fa762aa9"
    );
    assert_eq!(
        mix.details["artists"],
        json!(["c0b2500e-0cef-4130-869d-732b23ed9df5"])
    );
    // Without genres of its own: its release group's.
    let single = row(&rows, &format!("recording/{SINGLE}"));
    assert_eq!(sorted(single.labels.clone()), ["alternative-rock", "rock"]);
    // The links people share for a track: its Spotify, Deezer, Apple Music and TIDAL pages.
    let peter_pan = row(&rows, &format!("recording/{PETER_PAN}"));
    assert!(peter_pan
        .keys
        .contains(&link_key(PETER_PAN_SPOTIFY).unwrap()));
    assert!(peter_pan
        .keys
        .contains(&link_key("https://www.deezer.com/track/2434824285").unwrap()));
    assert_eq!(
        peter_pan
            .keys
            .iter()
            .filter(|k| k.starts_with("url:"))
            .count(),
        5
    );
    // A recording on no release.
    let live = row(&rows, "recording/e8f8ba70-45f7-4dac-a39a-fc0120249157");
    assert_eq!(live.fame, 0.0);
    assert!(live.details.get("release_group").is_none());

    // An artist: its pages on music services, its Wikidata item, its website as a related link.
    let amc = row(&rows, &format!("artist/{AMC}"));
    assert_eq!(amc.r#type, "MusicGroup");
    assert_eq!(amc.name, "American Music Club");
    assert!(
        amc.description.as_deref().unwrap().starts_with("Group"),
        "{:?}",
        amc.description
    );
    assert_eq!(
        sorted(amc.labels.clone()),
        ["americana", "folk-rock", "indie-rock", "slowcore"]
    );
    assert!(amc.keys.contains(&"wikidata:Q2458127".to_owned()));
    assert!(amc.keys.contains(&format!("mb:artist:{AMC}")));
    for page in [
        "https://open.spotify.com/artist/6uHjKRsYGaElOrKWlSEji3",
        "https://www.discogs.com/artist/92581",
    ] {
        assert!(
            amc.keys.contains(&link_key(page).unwrap()),
            "{page}: {:?}",
            amc.keys
        );
    }
    assert_eq!(amc.related_uris, ["http://www.americanmusicclub.com/"]);
    // Credited by no release in the fixture.
    assert_eq!(amc.fame, 0.0);

    // A release group: its Wikidata item, its Discogs master, its Wikipedia article.
    let rosemary = row(&rows, &format!("release-group/{ROSEMARY}"));
    assert_eq!(rosemary.r#type, "MusicAlbum");
    assert_eq!(
        rosemary.name,
        "Rosemary Clooney Sings the Music of Cole Porter"
    );
    assert!(rosemary
        .description
        .as_deref()
        .unwrap()
        .starts_with("Album by Rosemary Clooney"));
    assert_eq!(sorted(rosemary.labels.clone()), ["jazz", "swing"]);
    assert!(rosemary.keys.contains(&"wikidata:Q7368332".to_owned()));
    assert!(rosemary
        .keys
        .contains(&link_key("https://www.discogs.com/master/361950").unwrap()));
    assert_eq!(
        rosemary.details["wikipedia"],
        "https://en.wikipedia.org/wiki/Rosemary_Clooney_Sings_the_Music_of_Cole_Porter"
    );
    // Every card is one `Card::checked` accepts again unchanged, and every label a tag label.
    for r in &rows {
        assert!(r
            .labels
            .iter()
            .all(|l| pinakes_core::catalogue::label(l).as_deref() == Some(l)));
        assert!(
            r.keys.iter().all(|k| pinakes_core::catalogue::valid_key(k)),
            "{:?}",
            r.keys
        );
    }
}

/// The same dump read again by an update writes nothing; a new dump writes only the cards that
/// changed or are new, and removes those it no longer has: every other row stays as it was.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_writes_only_what_changed(pool: PgPool) {
    let s = setup(&pool, None);
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    let before = rows(&pool).await;
    // The same dump: nothing to do.
    let state = run(&s, &a, Run::Update).await.unwrap();
    assert_eq!(rows(&pool).await, before);
    assert!(state.last_update_at.is_some());
    // A new dump: one artist renamed, one release group gone, one recording more.
    let b = dump(s.root.path(), B, |entity, lines| match entity {
        "artist" => {
            for l in lines.iter_mut().filter(|l| l["id"] == ARTIFACTS) {
                l["name"] = json!("Artifacts (US)");
            }
        }
        "release-group" => lines.retain(|l| l["id"] != TRITONIS),
        "recording" => {
            let mut more = lines[0].clone();
            more["id"] = json!("00000000-1111-4222-8333-444444444444");
            more["title"] = json!("A New Recording");
            // An ISRC of its own: sharing the other's would take that one's key.
            more["isrcs"] = json!([]);
            lines.push(more);
        }
        _ => {}
    });
    let state = run(&s, &b, Run::Update).await.unwrap();
    let after = rows(&pool).await;
    assert_eq!(state.cards, 13);
    assert_eq!(state.position, Some(json!({ "version": B })));
    assert_eq!(state.upstream_version.as_deref(), Some(B));
    let renamed = row(&after, &format!("artist/{ARTIFACTS}"));
    assert_eq!(renamed.name, "Artifacts (US)");
    assert!(after
        .iter()
        .all(|r| r.source_id != format!("release-group/{TRITONIS}")));
    assert_eq!(
        row(&after, "recording/00000000-1111-4222-8333-444444444444").name,
        "A New Recording"
    );
    // Every other card is the same row version: not written at all (no stamp either).
    for old in &before {
        if old.source_id == format!("artist/{ARTIFACTS}")
            || old.source_id == format!("release-group/{TRITONIS}")
        {
            continue;
        }
        assert_eq!(row(&after, &old.source_id), old, "{}", old.source_id);
    }
    // The text index takes the update's changes through its queue.
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM search_index_catalogue_queue")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(queued, 3);
}

/// An ISRC on two recordings of the dump names neither: no card takes it as a key, and its
/// MusicBrainz page joins no card.
#[sqlx::test(migrations = "../../migrations")]
async fn an_isrc_on_two_recordings_keys_neither(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, |entity, lines| {
        if entity == "release" {
            for release in lines.iter_mut() {
                for medium in release["media"].as_array_mut().unwrap() {
                    for track in medium["tracks"].as_array_mut().unwrap() {
                        if track["recording"]["id"] == SINGLE {
                            track["recording"]["isrcs"] = json!(["USSM10211499"]);
                        }
                    }
                }
            }
        }
    });
    run(&s, &dir, Run::Import).await.unwrap();
    let rows = rows(&pool).await;
    for id in [MIX, SINGLE] {
        let r = row(&rows, &format!("recording/{id}"));
        assert!(
            !r.keys.contains(&"isrc:USSM10211499".to_owned()),
            "{:?}",
            r.keys
        );
    }
    // Another recording's own ISRC stays a key.
    assert!(row(&rows, &format!("recording/{PETER_PAN}"))
        .keys
        .contains(&"isrc:USSM10210913".to_owned()));
    let cfg = queue_cfg();
    let page = tagged(&pool, "https://musicbrainz.org/isrc/USSM10211499").await;
    join::referenced(&pool, &cfg, std::slice::from_ref(&page)).await;
    assert_eq!(backing(&pool, &page).await, None);
}

/// `limit` imports the first releases whose recordings, release groups and artists fit, then only
/// those release groups and artists; recordings on no release are not read.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limit_imports_the_first_releases_that_fit(pool: PgPool) {
    // The first release: 3 recordings, its release group and its artist make 5 items; the second
    // edition adds none; Arovane's album (6 more) does not fit.
    let s = setup(&pool, Some(5));
    let dir = dump(s.root.path(), B, |entity, lines| {
        if entity == "release" {
            // The two editions first, then Arovane.
            lines.sort_by_key(|l| l["title"] != "A Sorta Fairytale");
        }
    });
    let state = run(&s, &dir, Run::Import).await.unwrap();
    let ids: Vec<String> = rows(&pool).await.into_iter().map(|r| r.source_id).collect();
    // The release group and the artist of the single are not in the fixture's files.
    assert_eq!(
        ids,
        [
            format!("recording/{SINGLE}"),
            format!("recording/{PETER_PAN}"),
            format!("recording/{MIX}"),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(state.cards, 3);
    // The fame counts both editions, which fit.
    assert_eq!(
        row(&rows(&pool).await, &format!("recording/{MIX}")).fame,
        fame(2.0)
    );
}

/// A file whose SHA-256 is not the dump's writes nothing: the run fails (`checksum`) before any
/// card is written.
#[sqlx::test(migrations = "../../migrations")]
async fn a_file_with_another_checksum_writes_nothing(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let broken: String = sums
        .lines()
        .map(|l| {
            if l.ends_with("artist.tar.xz") {
                format!("{}{}\n", "0".repeat(64), &l[64..])
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    std::fs::write(dir.join("SHA256SUMS"), broken).unwrap();
    let failed = run(&s, &dir, Run::Import).await.unwrap_err();
    assert!(failed.to_string().contains("checksum"), "{failed}");
    assert!(rows(&pool).await.is_empty());
    let state = store::state(&pool, Catalogue::Musicbrainz)
        .await
        .unwrap()
        .unwrap();
    assert!(state.last_error.unwrap().starts_with("checksum"));
    assert_eq!(state.error_count, 1);
}

/// An update that would remove more than a tenth of the cards stops (`removals`) and removes
/// none; a full import of the same dump then does.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_that_would_remove_too_much_removes_nothing(pool: PgPool) {
    let s = setup(&pool, None);
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    let b = dump(s.root.path(), B, |entity, lines| {
        if matches!(entity, "artist" | "release-group") {
            lines.clear();
        }
    });
    let failed = run(&s, &b, Run::Update).await.unwrap_err();
    assert!(failed.to_string().contains("removals"), "{failed}");
    assert_eq!(rows(&pool).await.len(), 13);
    let state = run(&s, &b, Run::Import).await.unwrap();
    assert_eq!((state.cards, rows(&pool).await.len()), (9, 9));
    assert_eq!(state.error_count, 1);
}

/// A full import cut off after some items resumes from its cursor: the items up to it are not
/// read again, those after it are, and none is removed at its end.
#[sqlx::test(migrations = "../../migrations")]
async fn a_cut_off_import_resumes_after_its_cursor(pool: PgPool) {
    let s = setup(&pool, None);
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    // As a second full import cut off after its first artist has it: under way in generation 2,
    // that artist written in it, its cursor after that artist.
    sqlx::query("UPDATE catalogue_state SET run='import',phase='import',generation=2,cursor=$1 WHERE catalogue='musicbrainz'")
        .bind(json!({"version": B, "phase": "write", "after": format!("artist/{AMC}"), "removed": 0, "as_of": null}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE catalogue_card SET seen_generation=2 WHERE source_id=$1")
        .bind(format!("artist/{AMC}"))
        .execute(&pool)
        .await
        .unwrap();
    // Both artists renamed in the dump the run resumes with.
    let b = dump(s.root.path(), B, |entity, lines| {
        if entity == "artist" {
            for l in lines.iter_mut() {
                l["name"] = json!(format!("{} (renamed)", l["name"].as_str().unwrap()));
            }
        }
    });
    let state = run(&s, &b, Run::Import).await.unwrap();
    let after = rows(&pool).await;
    assert_eq!((after.len(), state.cards), (13, 13));
    // The artist up to the cursor was not read again; the one after it was.
    assert_eq!(
        row(&after, &format!("artist/{AMC}")).name,
        "American Music Club"
    );
    assert_eq!(
        row(&after, &format!("artist/{ARTIFACTS}")).name,
        "Artifacts (renamed)"
    );
    assert_eq!(state.generation, 2);
}

fn queue_cfg() -> QueueConfig {
    QueueConfig {
        catalogues: join::joinable(&Default::default()),
        ..Default::default()
    }
}
async fn tagged(pool: &PgPool, uri: &str) -> String {
    let id = pinakes_core::store::ensure_resource(pool, uri)
        .await
        .unwrap();
    sqlx::query("UPDATE resource SET orphaned_at=NULL WHERE id=$1")
        .bind(&id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO reference(resource_id,kind,author,label,object_path,created_at,unit_key) VALUES($1,'tag',$2,'t','/pub/t/'||$1,now(),'tag:t') ON CONFLICT DO NOTHING")
        .bind(&id)
        .bind("y".repeat(52))
        .execute(pool)
        .await
        .unwrap();
    id
}
/// The `source_id` of the card a resource is built from.
async fn backing(pool: &PgPool, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT c.source_id FROM resource r JOIN catalogue_card c ON c.id=r.catalogue_card WHERE r.id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// Shared links join their cards: a musicbrainz.org page, a streaming link MusicBrainz relates
/// to the recording, and the MusicBrainz page of a recording's ISRC.
#[sqlx::test(migrations = "../../migrations")]
async fn shared_links_join_their_cards(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    run(&s, &dir, Run::Import).await.unwrap();
    let cfg = queue_cfg();
    let page = tagged(&pool, &format!("https://musicbrainz.org/recording/{MIX}")).await;
    let spotify = tagged(&pool, PETER_PAN_SPOTIFY).await;
    let isrc = tagged(&pool, "https://musicbrainz.org/isrc/USHMD1320901").await;
    let artist = tagged(
        &pool,
        "https://open.spotify.com/artist/6uHjKRsYGaElOrKWlSEji3",
    )
    .await;
    assert_eq!(
        join::referenced(
            &pool,
            &cfg,
            &[page.clone(), spotify.clone(), isrc.clone(), artist.clone()]
        )
        .await,
        4
    );
    assert_eq!(
        backing(&pool, &page).await,
        Some(format!("recording/{MIX}"))
    );
    assert_eq!(
        backing(&pool, &spotify).await,
        Some(format!("recording/{PETER_PAN}"))
    );
    assert_eq!(
        backing(&pool, &isrc).await,
        Some(format!("recording/{TABL_F}"))
    );
    assert_eq!(backing(&pool, &artist).await, Some(format!("artist/{AMC}")));
    let doc: Value = sqlx::query_scalar("SELECT doc FROM resource WHERE id=$1")
        .bind(&spotify)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(doc["name"], "Operation Peter Pan");
    assert_eq!(doc["type"], "MusicRecording");
    assert_eq!(doc["catalogue"]["catalogue"], "musicbrainz");
}

/// Search finds the imported cards by their words, and none once the catalogue is off.
#[sqlx::test(migrations = "../../migrations")]
async fn search_finds_the_cards_until_the_catalogue_is_off(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    run(&s, &dir, Run::Import).await.unwrap();
    let on = AppState::new(pool.clone(), Config::default()).unwrap();
    let index = on.search_index.clone();
    let on = api::router(on);
    let found = common::get(&on, "/v1/search?q=operation%20peter%20pan").await;
    let names: Vec<&str> = found["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(names, ["Operation Peter Pan"], "{found}");
    let tagged = common::get(&on, "/v1/search?tags=slowcore").await;
    assert_eq!(tagged["items"][0]["name"], "American Music Club");
    let mut config = Config::default();
    config.catalogues.musicbrainz.enabled = false;
    let mut off = AppState::new(pool.clone(), config).unwrap();
    off.search_index = index;
    let off = api::router(off);
    let found = common::get(&off, "/v1/search?q=operation%20peter%20pan").await;
    assert!(found["items"].as_array().unwrap().is_empty(), "{found}");
}

/// The daemon runs the MusicBrainz importer while the catalogue is on, and only then.
#[test]
fn the_daemon_runs_the_importer_while_the_catalogue_is_on() {
    let mut config = Config::default();
    let started = run::to_start(run::importers(&config), &config);
    assert_eq!(
        started.iter().map(|i| i.catalogue()).collect::<Vec<_>>(),
        [Catalogue::Musicbrainz]
    );
    config.catalogues.musicbrainz.enabled = false;
    assert!(run::to_start(run::importers(&config), &config).is_empty());
}
