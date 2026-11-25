//! `data/spellings.json`, the URI-only spelling rules' test vectors (docs/api.md, Spelling
//! vectors): every vector is what this build answers for its link, the file keeps its one
//! layout (one vector per line), every root is its own root, and every rule has vectors. Its
//! identifier pages likewise: each is what this build answers, an address of its identifier's
//! card, and every source with roots of its own scheme has them.
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
/// An identifier page: the web page `alias::web_page` builds for an identifier, `null` when there
/// is none.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Page {
    id: String,
    page: Option<String>,
    rule: String,
}
#[derive(Deserialize)]
struct File {
    about: String,
    vectors: Vec<Vector>,
    pages: Vec<Page>,
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

/// What this build answers for an identifier: its page (`alias::web_page`) and the rule that
/// builds it, else no page and `none`.
fn page_answer(id: &str) -> Page {
    match alias::web_page(id) {
        Some((page, rule)) => Page {
            id: id.to_owned(),
            page: Some(page),
            rule: rule.name(),
        },
        None => Page {
            id: id.to_owned(),
            page: None,
            rule: "none".to_owned(),
        },
    }
}

/// The file's one layout: the vectors, then the pages, in their order, one per line.
fn render(about: &str, vectors: &[Vector], pages: &[Page]) -> String {
    fn lines<T: Serialize>(out: &mut String, all: &[T]) {
        for (i, v) in all.iter().enumerate() {
            out.push_str("    ");
            out.push_str(&serde_json::to_string(v).unwrap());
            out.push_str(if i + 1 < all.len() { ",\n" } else { "\n" });
        }
    }
    let mut out = format!(
        "{{\n  \"about\": {},\n  \"vectors\": [\n",
        serde_json::to_string(about).unwrap()
    );
    lines(&mut out, vectors);
    out.push_str("  ],\n  \"pages\": [\n");
    lines(&mut out, pages);
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
    let pages: Vec<Page> = file.pages.iter().map(|p| page_answer(&p.id)).collect();
    let wrong_pages: Vec<String> = file
        .pages
        .iter()
        .zip(&pages)
        .filter(|(p, a)| p != a)
        .map(|(p, a)| format!("{p:?}\n  this build: {a:?}"))
        .collect();
    let wrong: Vec<String> = wrong.into_iter().chain(wrong_pages).collect();
    let expected = render(&file.about, &answers, &pages);
    if expected != FILE {
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("spellings.json");
        std::fs::write(&out, &expected).unwrap();
        panic!(
            "data/spellings.json is not what this build answers ({} vectors or pages differ; the \
             rest is layout):\n{}\nThis build's file: {}; compare it, then copy it over \
             data/spellings.json.",
            wrong.len(),
            wrong
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
            out.display()
        );
    }
    let mut links: Vec<&str> = file.vectors.iter().map(|v| v.link.as_str()).collect();
    links.sort_unstable();
    let before = links.len();
    links.dedup();
    assert_eq!(links.len(), before, "a link has one vector");
    let mut ids: Vec<&str> = file.pages.iter().map(|p| p.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "an identifier has one page vector");
}

/// The root of a link as `alias::root` and `model::identity` name it.
fn folded(link: &str) -> String {
    answer(link).root
}

/// An identifier page is a link Pinakes keeps as written, and an address of the identifier's
/// card: a card whose root is no web link (`doi:`, a source's own scheme) folds the page back
/// to that root; a join key's page is its own root, the card's `uri`.
#[test]
fn every_page_is_an_address_of_its_card() {
    for p in file().pages {
        let Some(page) = &p.page else { continue };
        let (_, written, scheme) = model::identity(page).unwrap();
        assert_eq!(
            &written, page,
            "{p:?}: a page is written as Pinakes keeps it"
        );
        assert!(matches!(scheme.as_str(), "https"), "{p:?}: an https page");
        let root = folded(page);
        if p.rule == "doi" || p.rule.starts_with("source/") {
            assert_eq!(
                root,
                folded(&p.id),
                "{p:?}: the page folds to the card's root"
            );
            assert!(
                !root.starts_with("https:"),
                "{p:?}: a root of its own scheme"
            );
        } else {
            assert_eq!(&root, page, "{p:?}: a key's page is its own root");
            assert!(alias::rooted(page).is_none(), "{p:?}");
        }
    }
}

/// Every source whose module builds root pages (`fn root_page`) has page vectors, every source
/// root of the spelling vectors that is no web link has its page, and the key rules and `none`
/// have theirs.
#[test]
fn every_page_rule_has_vectors() {
    let file = file();
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for p in &file.pages {
        *count.entry(p.rule.as_str()).or_default() += 1;
    }
    let mut rules: Vec<String> = [Rule::Doi, Rule::Wikidata, Rule::MusicBrainz, Rule::Osm]
        .into_iter()
        .map(|r| r.name().to_owned())
        .collect();
    rules.push("none".to_owned());
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sources");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        if !matches!(name.as_str(), "mod" | "kit")
            && std::fs::read_to_string(&path)
                .unwrap()
                .contains("fn root_page(&self")
        {
            rules.push(format!("source/{name}"));
        }
    }
    let missing: Vec<&String> = rules
        .iter()
        .filter(|r| !count.contains_key(r.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "page rules without a vector: {missing:?}"
    );
    // A source's root in a scheme of its own: the page a client links for its card.
    for v in &file.vectors {
        if v.rule.starts_with("source/") && !v.root.starts_with("https:") {
            let (_, rule) =
                alias::web_page(&v.root).unwrap_or_else(|| panic!("no page for the root of {v:?}"));
            assert_eq!(rule.name(), v.rule, "{v:?}");
        }
    }
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

/// The family of an `alias::Named` rule. A new family fails to compile here: give it vectors in
/// `every_rule_has_vectors`.
fn family(named: alias::Named) -> &'static str {
    match named {
        alias::Named::Core(_) => "core",
        alias::Named::Source(_) => "source",
        alias::Named::Music(_) => "music",
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
    // The families asked for below: core rules, official sources, music services.
    let families = [
        alias::Named::Core(Rule::Cursor),
        alias::Named::Source("federal_register"),
        alias::Named::Music(Service::Spotify),
    ]
    .map(family);
    assert_eq!(families, ["core", "source", "music"]);
    let mut rules: Vec<String> = core_rules()
        .into_iter()
        .map(|r| r.name().to_owned())
        .collect();
    rules.extend(["identity", "none"].map(String::from));
    // Discogs links are mostly the discogs source's (music.rs roots them the same way, and a
    // removal of the source hands its vectors over), but music.rs reads some the source does not
    // (a port), so every service has vectors of its own.
    rules.extend(
        services()
            .into_iter()
            .map(|s| format!("music/{}", s.token())),
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

/// `scripts/check-removals` rewrites the file when it removes a source (its vectors and its
/// pages): it writes this file's layout byte for byte.
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
