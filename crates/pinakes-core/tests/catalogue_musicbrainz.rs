//! The MusicBrainz importer over dumps packed from real dump lines
//! (`fixtures/catalogue/musicbrainz`): the cards it makes, a new dump's update (only what
//! changed is written), the ISRC rule, `limit`, the checksum and removal guards, runs that
//! resume, the mirror over HTTP, joins of shared links and search with the catalogue on and off.
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
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
const ENTITIES: [&str; 4] = ["artist", "release-group", "release", "recording"];
/// The cards the fixture makes: 4 artists, 4 release groups, 3 + 4 recordings on the releases and
/// 2 on none.
const CARDS: usize = 17;
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
const TORI_AMOS: &str = "c0b2500e-0cef-4130-869d-732b23ed9df5";
const SORTA_GROUP: &str = "51e8835b-98de-4cf9-aae1-6315fa762aa9";
const CYCLIPH_GROUP: &str = "fec49937-f68a-3c94-b3db-814de1e4a712";

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
fn sha256(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// How a test dump is packed.
struct Pack<'a> {
    edit: &'a dyn Fn(&str, &mut Vec<Value>),
    /// Raw lines added at the end of an entity's file.
    raw: &'a dyn Fn(&str) -> String,
    /// An entity's `TIMESTAMP`.
    timestamp: &'a dyn Fn(&str) -> String,
    /// Bytes of an entry after `mbdump/<entity>`.
    after: Option<Vec<u8>>,
}
impl Default for Pack<'_> {
    fn default() -> Self {
        Self {
            edit: &|_, _| {},
            raw: &|_| String::new(),
            timestamp: &|_| TIMESTAMP.to_owned(),
            after: None,
        }
    }
}
/// A dump `<root>/<version>/` of the fixture lines, packed as `pack` says, with its
/// `SHA256SUMS`.
fn pack(root: &Path, version: &str, pack: Pack) -> PathBuf {
    let dir = root.join(version);
    std::fs::create_dir_all(&dir).unwrap();
    let mut sums = String::new();
    for entity in ENTITIES {
        let mut entities = lines(entity);
        (pack.edit)(entity, &mut entities);
        let mut body: String = entities.iter().map(|e| format!("{e}\n")).collect();
        body.push_str(&(pack.raw)(entity));
        let timestamp = (pack.timestamp)(entity);
        let path = format!("mbdump/{entity}");
        let mut files: Vec<(&str, &[u8])> = vec![
            ("TIMESTAMP", timestamp.as_bytes()),
            ("README", b"one entity per line\n"),
            (&path, body.as_bytes()),
        ];
        if let Some(after) = &pack.after {
            files.push(("mbdump/more", after));
        }
        let packed = xz(&tar(&files));
        let name = format!("{entity}.tar.xz");
        sums.push_str(&format!("{} *{name}\n", sha256(&packed)));
        std::fs::write(dir.join(name), packed).unwrap();
    }
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    dir
}
fn dump(root: &Path, version: &str, edit: impl Fn(&str, &mut Vec<Value>)) -> PathBuf {
    pack(
        root,
        version,
        Pack {
            edit: &edit,
            ..Pack::default()
        },
    )
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
fn importer(s: &Setup, source: Source) -> MusicBrainz {
    MusicBrainz::new(source, "")
        .unwrap()
        .threads(2)
        .configured(&s.ctx.config)
        .retry_wait(Duration::from_millis(10))
}
async fn run_with(s: &Setup, importer: MusicBrainz, kind: Run) -> anyhow::Result<store::State> {
    run::run_once(Arc::new(importer), &s.ctx, kind, |_| {}).await
}
async fn run(s: &Setup, dir: &Path, kind: Run) -> anyhow::Result<store::State> {
    run_with(s, importer(s, Source::Dir(dir.to_owned())), kind).await
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
async fn reset(pool: &PgPool) {
    sqlx::query("DELETE FROM catalogue_card")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM catalogue_state")
        .execute(pool)
        .await
        .unwrap();
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
    assert_eq!(
        rows.len(),
        CARDS,
        "{:#?}",
        rows.iter().map(|r| &r.source_id).collect::<Vec<_>>()
    );
    assert_eq!(state.cards, CARDS as i64);
    assert_eq!(state.upstream_version.as_deref(), Some(B));
    assert_eq!(state.position, Some(json!({ "version": B, "limit": null })));
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
    assert_eq!(mix.details["release_group"], SORTA_GROUP);
    assert_eq!(mix.details["artists"], json!([TORI_AMOS]));
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
    // A person, credited by both editions of her single.
    let tori = row(&rows, &format!("artist/{TORI_AMOS}"));
    assert_eq!((tori.r#type.as_str(), tori.fame), ("Person", fame(2.0)));

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
    assert_eq!(rosemary.fame, 0.0);
    // The single's release group: its two editions give its fame.
    assert_eq!(
        row(&rows, &format!("release-group/{SORTA_GROUP}")).fame,
        fame(2.0)
    );
    // Arovane's EP: its release's YouTube Music album is one of its links, the release's Discogs
    // page (an edition) is not, its own Discogs master is.
    let cycliph = row(&rows, &format!("release-group/{CYCLIPH_GROUP}"));
    let album = pinakes_core::music::parse(
        "https://music.youtube.com/playlist?list=OLAK5uy_nWxQaKo6rBMy0uVC7W-AyFgQADBLmyAus",
    )
    .unwrap()
    .canonical;
    assert!(
        cycliph.keys.contains(&link_key(&album).unwrap()),
        "{:?}",
        cycliph.keys
    );
    assert!(!cycliph
        .keys
        .contains(&link_key("https://www.discogs.com/release/55742").unwrap()));
    assert!(cycliph
        .keys
        .contains(&link_key("https://www.discogs.com/master/433774").unwrap()));
    assert_eq!(cycliph.fame, fame(1.0));
    // Every label a tag label, every key valid.
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
    assert_eq!(state.cards, CARDS as i64);
    assert_eq!(state.position, Some(json!({ "version": B, "limit": null })));
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
    // Nothing is left in the catalogue's directory.
    let left: Vec<_> = std::fs::read_dir(s.ctx.config.catalogues.work_dir(Catalogue::Musicbrainz))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
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
/// those release groups and artists (the reads stop once they have them all: the garbage after
/// them is never read); recordings on no release are not read.
#[sqlx::test(migrations = "../../migrations")]
async fn a_limit_imports_the_first_releases_that_fit(pool: PgPool) {
    // The first release: 3 recordings, its release group and its artist make 5 items; the second
    // edition adds none; Arovane's EP (6 more) does not fit.
    let s = setup(&pool, Some(5));
    let edit = |entity: &str, lines: &mut Vec<Value>| match entity {
        // The two editions first, then Arovane.
        "release" => lines.sort_by_key(|l| l["title"] != "A Sorta Fairytale"),
        // The wanted ones first.
        "release-group" => lines.sort_by_key(|l| l["id"] != SORTA_GROUP),
        "artist" => lines.sort_by_key(|l| l["id"] != TORI_AMOS),
        _ => {}
    };
    let garbage = |entity: &str| {
        if matches!(entity, "release-group" | "artist") {
            "not json\n".repeat(150)
        } else {
            String::new()
        }
    };
    let dir = pack(
        s.root.path(),
        B,
        Pack {
            edit: &edit,
            raw: &garbage,
            ..Pack::default()
        },
    );
    let state = run(&s, &dir, Run::Import).await.unwrap();
    let ids: Vec<String> = rows(&pool).await.into_iter().map(|r| r.source_id).collect();
    assert_eq!(
        ids,
        [
            format!("artist/{TORI_AMOS}"),
            format!("recording/{SINGLE}"),
            format!("recording/{PETER_PAN}"),
            format!("recording/{MIX}"),
            format!("release-group/{SORTA_GROUP}"),
        ]
    );
    assert_eq!(state.cards, 5);
    assert_eq!(state.position, Some(json!({ "version": B, "limit": 5 })));
    // The fame counts both editions, which fit.
    assert_eq!(
        row(&rows(&pool).await, &format!("recording/{MIX}")).fame,
        fame(2.0)
    );
}

/// A configured limit that changes hands an update over to a full import: a different subset is
/// a new import, not a change of the old one.
#[sqlx::test(migrations = "../../migrations")]
async fn a_changed_limit_is_a_full_import(pool: PgPool) {
    let s = setup(&pool, Some(5));
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    assert_eq!(rows(&pool).await.len(), 5);
    let unlimited = setup(&pool, None);
    let b = dump(unlimited.root.path(), B, unchanged);
    let state = run(&unlimited, &b, Run::Update).await.unwrap();
    assert_eq!((state.cards, state.generation), (CARDS as i64, 2));
    assert_eq!(state.import_requested, None);
    assert!(state.last_import_at.is_some());
}

/// A file whose SHA-256 is not the dump's writes nothing: the run fails (`checksum`) before any
/// card is written; so does a file the dump's list leaves out. The checksum covers the whole
/// file, also what follows the entry read. A local dump without a list is read unchecked.
#[sqlx::test(migrations = "../../migrations")]
async fn checksums_cover_whole_files(pool: PgPool) {
    let s = setup(&pool, None);
    // 3 MB that don't compress, after the entry read.
    let mut noise = vec![0u8; 3 << 20];
    let mut seed = 1u64;
    for b in noise.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (seed >> 56) as u8;
    }
    let dir = pack(
        s.root.path(),
        A,
        Pack {
            after: Some(noise),
            ..Pack::default()
        },
    );
    run(&s, &dir, Run::Import).await.unwrap();
    assert_eq!(rows(&pool).await.len(), CARDS);

    reset(&pool).await;
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
    std::fs::write(dir.join("SHA256SUMS"), &broken).unwrap();
    let failed = run(&s, &dir, Run::Import).await.unwrap_err();
    assert!(failed.to_string().contains("checksum"), "{failed}");
    assert!(rows(&pool).await.is_empty());
    let state = store::state(&pool, Catalogue::Musicbrainz)
        .await
        .unwrap()
        .unwrap();
    assert!(state.last_error.unwrap().starts_with("checksum"));
    assert_eq!(state.error_count, 1);
    // A list that leaves a file out.
    let partial: String = sums
        .lines()
        .filter(|l| !l.ends_with("recording.tar.xz"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(dir.join("SHA256SUMS"), partial).unwrap();
    let failed = run(&s, &dir, Run::Import).await.unwrap_err();
    assert!(failed.to_string().contains("checksum"), "{failed}");
    assert!(rows(&pool).await.is_empty());
    // No list: read unchecked.
    std::fs::remove_file(dir.join("SHA256SUMS")).unwrap();
    run(&s, &dir, Run::Import).await.unwrap();
    assert_eq!(rows(&pool).await.len(), CARDS);
}

/// More than 100 lines that are not entities, and more than one in a thousand, fail the run
/// (`parse`) and write nothing.
#[sqlx::test(migrations = "../../migrations")]
async fn a_file_of_bad_lines_fails(pool: PgPool) {
    let s = setup(&pool, None);
    let bad = |entity: &str| {
        if entity == "release" {
            "{not an entity\n".repeat(101)
        } else {
            String::new()
        }
    };
    let dir = pack(
        s.root.path(),
        B,
        Pack {
            raw: &bad,
            ..Pack::default()
        },
    );
    let failed = run(&s, &dir, Run::Import).await.unwrap_err();
    assert!(failed.to_string().contains("parse"), "{failed}");
    assert!(rows(&pool).await.is_empty());
}

/// The dump's time is the earliest of its files' `TIMESTAMP`s.
#[sqlx::test(migrations = "../../migrations")]
async fn the_dump_is_as_current_as_its_earliest_file(pool: PgPool) {
    let s = setup(&pool, None);
    let timestamp = |entity: &str| {
        if entity == "artist" {
            "2026-11-24 23:00:00.5+00\n".to_owned()
        } else {
            TIMESTAMP.to_owned()
        }
    };
    let dir = pack(
        s.root.path(),
        B,
        Pack {
            timestamp: &timestamp,
            ..Pack::default()
        },
    );
    let state = run(&s, &dir, Run::Import).await.unwrap();
    assert_eq!(
        state.current_as_of.unwrap().to_rfc3339(),
        "2026-11-24T23:00:00.500+00:00"
    );
}

/// An update that would remove more than a tenth of the cards stops (`removals`, with the counts)
/// before it writes or removes anything, and waits: tried again with the same newest dump, it
/// reads nothing; a full import asked for (`--request`) then imports the dump and removes what it
/// no longer has.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_that_would_remove_too_much_waits(pool: PgPool) {
    let s = setup(&pool, None);
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    let before = rows(&pool).await;
    let shrunk = |entity: &str, lines: &mut Vec<Value>| {
        if matches!(entity, "artist" | "release-group") {
            lines.clear();
        }
        // A change that would be written, were the update not refused.
        if entity == "recording" {
            lines[0]["title"] = json!("Renamed");
        }
    };
    let b = dump(s.root.path(), B, shrunk);
    let failed = run(&s, &b, Run::Update).await.unwrap_err();
    assert!(failed.to_string().contains("removals"), "{failed}");
    assert!(failed.to_string().contains("8 of 17 cards"), "{failed}");
    assert_eq!(rows(&pool).await, before);
    // Tried again with its files gone: refused at once, without opening any.
    for e in ENTITIES {
        std::fs::remove_file(b.join(format!("{e}.tar.xz"))).unwrap();
    }
    let failed = run(&s, &b, Run::Update).await.unwrap_err();
    assert!(failed.to_string().contains("removals"), "{failed}");
    assert_eq!(rows(&pool).await, before);
    // A full import asked for: the update hands over to it.
    let b = dump(s.root.path(), B, shrunk);
    run::request_import(
        &pool,
        Catalogue::Musicbrainz,
        pinakes_core::catalogue::musicbrainz::REQUESTED,
    )
    .await
    .unwrap();
    let state = run(&s, &b, Run::Update).await.unwrap();
    assert_eq!((state.cards, rows(&pool).await.len()), (9, 9));
    assert_eq!((state.import_requested, state.generation), (None, 2));
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
    // Every artist renamed in the dump the run resumes with.
    let b = dump(s.root.path(), B, |entity, lines| {
        if entity == "artist" {
            for l in lines.iter_mut() {
                l["name"] = json!(format!("{} (renamed)", l["name"].as_str().unwrap()));
            }
        }
    });
    let state = run(&s, &b, Run::Import).await.unwrap();
    let after = rows(&pool).await;
    assert_eq!((after.len(), state.cards), (CARDS, CARDS as i64));
    // The artist up to the cursor was not read again; those after it were.
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

/// Recordings made up from a fixture line, with IDs that sort before the others.
fn extra_recordings(n: usize) -> Vec<Value> {
    let base = &lines("recording")[0];
    (0..n)
        .map(|i| {
            let mut r = base.clone();
            r["id"] = json!(format!("00000000-0000-4000-8000-{i:012}"));
            r["title"] = json!(format!("Extra {i}"));
            r["isrcs"] = json!([]);
            r
        })
        .collect()
}

/// A run cut off in its writes resumes from the sorted runs its read finished, merged down in
/// passes: the dump is not read again (here it is gone), and the cards are written from the
/// cursor on.
#[sqlx::test(migrations = "../../migrations")]
async fn a_run_cut_off_in_its_writes_resumes_from_its_sorted_runs(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, |entity, lines| {
        if entity == "recording" {
            lines.extend(extra_recordings(1500));
        }
    });
    // Writes fail at an item of the second part.
    sqlx::query("CREATE FUNCTION refuse_one() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.source_id='recording/00000000-0000-4000-8000-000000001200' THEN RAISE EXCEPTION 'refused'; END IF; RETURN NEW; END $$")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER refuse_one BEFORE INSERT ON catalogue_card FOR EACH ROW EXECUTE FUNCTION refuse_one()")
        .execute(&pool)
        .await
        .unwrap();
    // A small sort memory and fan-in: many runs, merged in passes.
    let small = || importer(&s, Source::Dir(dir.clone())).sorting(4096, 3);
    assert!(run_with(&s, small(), Run::Import).await.is_err());
    let written: i64 = sqlx::query_scalar("SELECT count(*) FROM catalogue_card")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(written, 1000);
    sqlx::query("DROP TRIGGER refuse_one ON catalogue_card")
        .execute(&pool)
        .await
        .unwrap();
    for e in ENTITIES {
        std::fs::remove_file(dir.join(format!("{e}.tar.xz"))).unwrap();
    }
    let state = run_with(&s, small(), Run::Import).await.unwrap();
    assert_eq!(state.cards, (CARDS + 1500) as i64);
}

/// An update cut off in its removals goes on with the rest of its list, and needs no disk and no
/// dump.
#[sqlx::test(migrations = "../../migrations")]
async fn an_update_cut_off_in_its_removals_removes_the_rest(pool: PgPool) {
    let s = setup(&pool, None);
    let a = dump(s.root.path(), A, unchanged);
    run(&s, &a, Run::Import).await.unwrap();
    let work = s.ctx.config.catalogues.work_dir(Catalogue::Musicbrainz);
    std::fs::create_dir_all(&work).unwrap();
    let ids = [
        format!("artist/{AMC}"),
        format!("artist/{ARTIFACTS}"),
        format!("release-group/{TRITONIS}"),
    ];
    std::fs::write(work.join(format!("removals-{B}")), ids.join("\n") + "\n").unwrap();
    sqlx::query("UPDATE catalogue_state SET run='update',phase='update',cursor=$1 WHERE catalogue='musicbrainz'")
        .bind(json!({"version": B, "phase": "remove", "after": null, "removed": 1, "as_of": null}))
        .execute(&pool)
        .await
        .unwrap();
    // No free disk at all, and the dump's files gone.
    let mut tight = setup(&pool, None);
    tight.ctx = Context {
        free_disk: Arc::new(|_| Ok(0)),
        ..s.ctx.clone()
    };
    for e in ENTITIES {
        std::fs::remove_file(a.join(format!("{e}.tar.xz"))).unwrap();
    }
    let state = run(&tight, &a, Run::Update).await.unwrap();
    let left: Vec<String> = rows(&pool).await.into_iter().map(|r| r.source_id).collect();
    assert!(left.contains(&ids[0]));
    assert!(!left.contains(&ids[1]) && !left.contains(&ids[2]));
    assert_eq!(state.cards, CARDS as i64 - 2);
    assert_eq!(state.position, Some(json!({ "version": B, "limit": null })));
}

/// Many chunks of lines over several threads make the same cards as one thread.
#[sqlx::test(migrations = "../../migrations")]
async fn parallel_reads_make_the_same_cards(pool: PgPool) {
    let s = setup(&pool, None);
    let base = lines("release");
    let dir = dump(s.root.path(), B, |entity, lines| {
        if entity == "release" {
            // About 14 MB of releases: several 4 MB chunks.
            for i in 0..900 {
                let mut r = base[i % base.len()].clone();
                r["id"] = json!(format!("00000000-0000-4000-9000-{i:012}"));
                for m in r["media"].as_array_mut().unwrap() {
                    for (n, t) in m["tracks"].as_array_mut().unwrap().iter_mut().enumerate() {
                        t["recording"]["id"] =
                            json!(format!("00000000-0000-4000-a000-{i:08}{n:04}"));
                        t["recording"]["isrcs"] = json!([]);
                    }
                }
                lines.push(r);
            }
        }
    });
    async fn hashes(pool: &PgPool) -> Vec<(String, Vec<u8>)> {
        sqlx::query_as(
            "SELECT source_id,content_hash FROM catalogue_card ORDER BY source_id COLLATE \"C\"",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }
    run_with(
        &s,
        importer(&s, Source::Dir(dir.clone())).threads(3),
        Run::Import,
    )
    .await
    .unwrap();
    let many = hashes(&pool).await;
    reset(&pool).await;
    run_with(
        &s,
        importer(&s, Source::Dir(dir.clone())).threads(1),
        Run::Import,
    )
    .await
    .unwrap();
    assert_eq!(hashes(&pool).await, many);
    assert!(many.len() > 3000, "{}", many.len());
}

/// One request the test mirror answered: the path, its `Range` and its `If-Range`.
type Request = (String, Option<String>, Option<String>);
/// A mirror over HTTP, as the daemon reads the official one: `LATEST`, `SHA256SUMS` and the four
/// files, each request recorded.
struct Mirror {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}
#[derive(Clone, Copy, PartialEq)]
enum Breaks {
    /// The release file's body breaks once, half way.
    Once,
    /// It breaks, and the rest is asked for from a file replaced meanwhile (a 200).
    Replaced,
    Never,
}
#[derive(Clone)]
struct Served {
    files: Arc<BTreeMap<String, Vec<u8>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    broke: Arc<Mutex<bool>>,
    latest: String,
    breaks: Breaks,
}
async fn serve(
    axum::extract::State(s): axum::extract::State<Served>,
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::{body::Body, http::StatusCode, response::Response};
    let header = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    s.requests
        .lock()
        .unwrap()
        .push((path.clone(), header("range"), header("if-range")));
    if path == "LATEST" {
        return Response::new(Body::from(format!("{}\n", s.latest)));
    }
    let Some(body) = s.files.get(&path).cloned() else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    };
    let etag = format!("\"{}\"", sha256(&body));
    if let Some(range) = header("range") {
        let from: usize = range
            .trim_start_matches("bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap();
        if s.breaks == Breaks::Replaced {
            return Response::builder()
                .header("etag", "\"another\"")
                .body(Body::from(body))
                .unwrap();
        }
        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("etag", etag)
            .header(
                "content-range",
                format!("bytes {from}-{}/{}", body.len() - 1, body.len()),
            )
            .body(Body::from(body[from..].to_vec()))
            .unwrap();
    }
    let break_now = path.ends_with("/release.tar.xz") && s.breaks != Breaks::Never && {
        let mut broke = s.broke.lock().unwrap();
        let first = !*broke;
        *broke = true;
        first
    };
    let response = Response::builder().header("etag", etag);
    if break_now {
        // Half the body, then (once the client has it) the connection breaks.
        let half = bytes::Bytes::from(body[..body.len() / 2].to_vec());
        let stream = futures_util::stream::unfold(0, move |step| {
            let half = half.clone();
            async move {
                match step {
                    0 => Some((Ok::<_, std::io::Error>(half), 1)),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Some((Err(std::io::Error::other("connection reset")), 2))
                    }
                    _ => None,
                }
            }
        });
        return response
            .header("content-length", body.len())
            .body(Body::from_stream(stream))
            .unwrap();
    }
    response.body(Body::from(body)).unwrap()
}
async fn mirror(files: BTreeMap<String, Vec<u8>>, latest: &str, breaks: Breaks) -> Mirror {
    let requests = Arc::new(Mutex::new(vec![]));
    let served = Served {
        files: Arc::new(files),
        requests: requests.clone(),
        broke: Arc::new(Mutex::new(false)),
        latest: latest.to_owned(),
        breaks,
    };
    let app = axum::Router::new()
        .route("/json-dumps/{*path}", axum::routing::get(serve))
        .with_state(served);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mirror {
        url: format!("http://{addr}/json-dumps/"),
        requests,
    }
}
fn served(dir: &Path, version: &str) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            (
                format!("{version}/{}", e.file_name().to_string_lossy()),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

/// The mirror's path: the newest dump from `LATEST`, its files checked against `SHA256SUMS`; a
/// body that breaks is asked for again from where it broke (`Range`, `If-Range` with its ETag);
/// an update when `LATEST` names the cards' dump asks for no file.
#[sqlx::test(migrations = "../../migrations")]
async fn the_mirror_is_read_and_a_broken_body_picked_up_where_it_broke(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    let m = mirror(served(&dir, B), B, Breaks::Once).await;
    let source = Source::parse(&m.url).unwrap();
    let state = run_with(&s, importer(&s, source.clone()), Run::Import).await;
    let requests = m.requests.lock().unwrap().clone();
    let state = state.unwrap_or_else(|e| panic!("{e}: {requests:?}"));
    assert_eq!(state.cards, CARDS as i64);
    assert_eq!(requests[0].0, "LATEST");
    let release: Vec<_> = requests
        .iter()
        .filter(|r| r.0 == format!("{B}/release.tar.xz"))
        .collect();
    assert_eq!(release.len(), 2, "{requests:?}");
    assert!(release[1].1.as_deref().unwrap().starts_with("bytes="));
    assert!(release[1].2.as_deref().unwrap().starts_with('"'));
    // The same dump again: an update reads `LATEST` and nothing more.
    m.requests.lock().unwrap().clear();
    run_with(&s, importer(&s, source), Run::Update)
        .await
        .unwrap();
    let requests = m.requests.lock().unwrap().clone();
    assert!(
        requests.iter().all(|r| !r.0.ends_with(".tar.xz")),
        "{requests:?}"
    );
}

/// A body whose rest comes from a file replaced meanwhile is a failure, never a spliced stream; a
/// file the mirror's list does not name is not downloaded at all.
#[sqlx::test(migrations = "../../migrations")]
async fn a_replaced_file_or_one_the_list_leaves_out_writes_nothing(pool: PgPool) {
    let s = setup(&pool, None);
    let dir = dump(s.root.path(), B, unchanged);
    let m = mirror(served(&dir, B), B, Breaks::Replaced).await;
    let failed = run_with(
        &s,
        importer(&s, Source::parse(&m.url).unwrap()),
        Run::Import,
    )
    .await
    .unwrap_err();
    assert!(failed.to_string().contains("download"), "{failed}");
    assert!(rows(&pool).await.is_empty());

    let mut files = served(&dir, B);
    let sums = String::from_utf8(files[&format!("{B}/SHA256SUMS")].clone()).unwrap();
    files.insert(
        format!("{B}/SHA256SUMS"),
        sums.lines()
            .filter(|l| !l.ends_with("artist.tar.xz"))
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .into_bytes(),
    );
    let m = mirror(files, B, Breaks::Never).await;
    let failed = run_with(
        &s,
        importer(&s, Source::parse(&m.url).unwrap()),
        Run::Import,
    )
    .await
    .unwrap_err();
    assert!(failed.to_string().contains("checksum"), "{failed}");
    let requests = m.requests.lock().unwrap().clone();
    assert!(
        requests.iter().all(|r| !r.0.ends_with("artist.tar.xz")),
        "{requests:?}"
    );
    assert!(rows(&pool).await.is_empty());
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

/// The daemon runs the MusicBrainz importer while the catalogue is on and the operator's contact
/// is set, and only then.
#[test]
fn the_daemon_runs_the_importer_while_the_catalogue_is_on() {
    let mut config = Config::default();
    assert!(run::importers(&config).is_empty());
    config.egress.contact = "ops@example.org".into();
    let started = run::to_start(run::importers(&config), &config);
    assert_eq!(
        started.iter().map(|i| i.catalogue()).collect::<Vec<_>>(),
        [Catalogue::Musicbrainz]
    );
    config.catalogues.musicbrainz.enabled = false;
    assert!(run::to_start(run::importers(&config), &config).is_empty());
}
