//! `data/spellings.json`, the URI-only spelling rules' test vectors (docs/api.md, Spelling
//! vectors): every vector is what this build answers for its link, the file keeps its one
//! layout (one vector per line), every root is its own root, and every rule has vectors.
use pinakes_core::{
    alias::{self, Rule},
    model,
    music::Service,
    sources,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const FILE: &str = include_str!("../../../data/spellings.json");

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Vector {
    link: String,
    root: String,
    basis: Option<String>,
    rule: String,
}
#[derive(Deserialize)]
struct File {
    about: String,
    vectors: Vec<Vector>,
}

fn file() -> File {
    serde_json::from_str(FILE).expect("data/spellings.json is JSON of the vectors' shape")
}

/// What this build answers for `link`: the root a URI-only rule names (`alias::rooted`), else the
/// link as `model::identity` writes it (`identity` when that differs from the link, `none` when
/// not).
fn answer(link: &str) -> Vector {
    if let Some((alias, rule)) = alias::rooted(link) {
        return Vector {
            link: link.to_owned(),
            root: alias.root,
            basis: Some(alias.basis.to_owned()),
            rule: rule.name(),
        };
    }
    let (_, normalized, _) =
        model::identity(link).unwrap_or_else(|e| panic!("{link:?} is no URI Pinakes keeps: {e}"));
    Vector {
        link: link.to_owned(),
        rule: if normalized == link {
            "none"
        } else {
            "identity"
        }
        .to_owned(),
        root: normalized,
        basis: None,
    }
}

/// The file's one layout: the vectors in their order, one per line.
fn render(about: &str, vectors: &[Vector]) -> String {
    let mut out = format!(
        "{{\n  \"about\": {},\n  \"vectors\": [\n",
        serde_json::to_string(about).unwrap()
    );
    for (i, v) in vectors.iter().enumerate() {
        out.push_str("    ");
        out.push_str(&serde_json::to_string(v).unwrap());
        out.push_str(if i + 1 < vectors.len() { ",\n" } else { "\n" });
    }
    out.push_str("  ]\n}\n");
    out
}

/// A rule change that changes an answer fails here until the file says the same, which also
/// changes the version (`/v1/info` `spelling_rules`). The file this build would write is left in
/// the test's temporary directory, to compare and copy.
#[test]
fn every_vector_is_what_this_build_answers() {
    let file = file();
    let wrong: Vec<String> = file
        .vectors
        .iter()
        .map(|v| (v, answer(&v.link)))
        .filter(|(v, a)| *v != a)
        .map(|(v, a)| format!("{v:?}\n  this build: {a:?}"))
        .collect();
    // This build's answers, each root without a vector of its own followed by one.
    let mut listed: std::collections::HashSet<String> =
        file.vectors.iter().map(|v| v.link.clone()).collect();
    let mut answers = vec![];
    for v in &file.vectors {
        let a = answer(&v.link);
        let root = listed.insert(a.root.clone()).then(|| answer(&a.root));
        answers.push(a);
        answers.extend(root);
    }
    let expected = render(&file.about, &answers);
    if expected != FILE {
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("spellings.json");
        std::fs::write(&out, &expected).unwrap();
        panic!(
            "data/spellings.json is not what this build answers ({} vectors differ; the rest is \
             layout):\n{}\nThis build's file: {}; compare it, then copy it over data/spellings.json.",
            wrong.len(),
            wrong.iter().take(20).cloned().collect::<Vec<_>>().join("\n"),
            out.display()
        );
    }
    let mut links: Vec<&str> = file.vectors.iter().map(|v| v.link.as_str()).collect();
    links.sort_unstable();
    let before = links.len();
    links.dedup();
    assert_eq!(links.len(), before, "a link has one vector");
}

/// A root is never itself an alias, nor written another way (an indexer folds a link once), and
/// has a vector of its own: a rule change that moves it shows in its own line.
#[test]
fn every_root_is_its_own_root() {
    let file = file();
    let links: std::collections::HashSet<&str> =
        file.vectors.iter().map(|v| v.link.as_str()).collect();
    for v in &file.vectors {
        assert_eq!(answer(&v.root).root, v.root, "{v:?}");
        assert!(alias::rooted(&v.root).is_none(), "{v:?}");
        assert!(
            links.contains(v.root.as_str()),
            "no vector of its own: {v:?}"
        );
    }
}

/// Every rule of `alias::Rule` (a new one fails to compile here until listed).
fn core_rules() -> Vec<Rule> {
    use Rule::*;
    let all = [
        Cursor,
        Osm,
        Isbn,
        Doi,
        Wikipedia,
        Wikidata,
        MusicBrainz,
        Tracking,
    ];
    for r in all {
        match r {
            Cursor | Osm | Isbn | Doi | Wikipedia | Wikidata | MusicBrainz | Tracking => {}
        }
    }
    all.to_vec()
}

/// Every music service `music::parse` reads (a new one fails to compile here until listed).
fn services() -> Vec<Service> {
    use Service::*;
    let all = [
        Spotify,
        AppleMusic,
        YouTube,
        Deezer,
        Tidal,
        Qobuz,
        SoundCloud,
        AmazonMusic,
        Bandcamp,
        Discogs,
    ];
    for s in all {
        match s {
            Spotify | AppleMusic | YouTube | Deezer | Tidal | Qobuz | SoundCloud | AmazonMusic
            | Bandcamp | Discogs => {}
        }
    }
    all.to_vec()
}

/// Every rule `alias::rooted` names has a vector that it roots: each `alias::Rule`, each music
/// service, and each registered source whose module spells its objects (`fn alias_root`); and
/// `identity` and `none` have theirs.
#[test]
fn every_rule_has_vectors() {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for v in file().vectors {
        *count.entry(v.rule).or_default() += 1;
    }
    let mut rules: Vec<String> = core_rules()
        .into_iter()
        .map(|r| r.name().to_owned())
        .collect();
    rules.extend(["identity", "none"].map(String::from));
    // A music service whose links an official source of its name spells first (Discogs) has that
    // source's vectors while the source is in the build; without it, music.rs roots the same
    // links to the same root, and a source's removal hands its vectors over (check-removals).
    rules.extend(
        services()
            .into_iter()
            .map(|s| s.token())
            .filter(|t| sources::by_name(t).is_none())
            .map(|t| format!("music/{t}")),
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sources");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        if matches!(name.as_str(), "mod" | "kit")
            || !std::fs::read_to_string(&path)
                .unwrap()
                .contains("fn alias_root(&self")
        {
            continue;
        }
        assert!(
            sources::by_name(&name).is_some(),
            "src/sources/{name}.rs spells links but no registered source is named {name}"
        );
        rules.push(format!("source/{name}"));
    }
    let missing: Vec<&String> = rules.iter().filter(|r| !count.contains_key(*r)).collect();
    assert!(missing.is_empty(), "rules without a vector: {missing:?}");
}

/// `scripts/check-removals` rewrites the file when it removes a source: it writes this file's
/// layout byte for byte.
#[test]
fn check_removals_writes_this_layout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(root.join("scripts/check-removals"))
        .arg("--spellings-layout")
        .arg(root.join("data/spellings.json"))
        .output()
        .expect("python3 runs scripts/check-removals");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
