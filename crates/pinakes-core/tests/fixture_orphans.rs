//! Every recorded fixture is read by a test. A file under `tests/fixtures` is named by a string
//! literal of the crate's code (its file name, or its path under `tests/fixtures`), or it is a
//! row's file in an index a test walks row by row (`ITERATED`), or its name is one a reader
//! builds (`FAMILIES`) from a part the reader spells. Anything else is an orphan: delete it with
//! its provenance row, or write the test it was recorded for.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Files that are no recorded answer: the directories' own notes and indexes.
const EXEMPT: &[&str] = &["README.md", "provenance.json"];

/// Pages kept as input for the HTML parser's tests, which read every `.html` under
/// `tests/fixtures` (`resolver::html`'s `a_parse_in_pieces_builds_the_same_tree`,
/// `tests/parse_worker.rs`): error pages no source test reads, whose markup the parser sees
/// nowhere else. Named one by one, so a new page no test reads is still an orphan.
const PARSER_INPUT: &[&str] = &[
    "sources/facebook/post-1.html",
    "sources/sec/sec-hdr-missing.html",
    "sources/substack/publication-missing.html",
    "sources/x/oembed-missing.html",
    "sources/x/profile-missing.html",
];

/// While `scripts/check-removals` runs the tests without a source (it sets this to the source),
/// a reader or an index the removal took is gone with its fixtures.
const REMOVING: &str = "PINAKES_REMOVING";

/// Whether a removal is checked: `REMOVING` set and not empty, as `test_support` reads it.
fn removing() -> bool {
    std::env::var(REMOVING).is_ok_and(|r| !r.is_empty())
}

/// A reader's code, without comments: a missing file fails, unless a removal is checked.
fn reader<'a>(code: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    let text = code.get(name).map(String::as_str);
    if text.is_none() && !removing() {
        panic!("{name}: no such file");
    }
    text
}

/// An index a test reads row by row, opening every row's file.
struct Iterated {
    /// Under `tests/fixtures`; a `*` segment is every directory there.
    index: &'static str,
    /// The row's key that names its file.
    key: &'static str,
    /// Rows the reader skips (by the end of the row's key): their file counts only where a
    /// literal names the row (`common::media::Recorded::with_recorded`).
    skipped: &'static [&'static str],
    /// The code that walks it; each must still name the index's file.
    readers: &'static [&'static str],
}
const ITERATED: &[Iterated] = &[
    // `Captured::busy`.
    Iterated {
        index: "hubs/provenance.json",
        key: "file",
        skipped: &[],
        readers: &["tests/editions.rs"],
    },
    // `recorded_but_busy` (every family), and the apps test's `Recorded::new`.
    Iterated {
        index: "lookups/*/provenance.json",
        key: "file",
        skipped: &[],
        readers: &["src/lookups/mod.rs", "tests/apps.rs"],
    },
    // `Recorded::new`: every family with a provenance.json.
    Iterated {
        index: "media/*/provenance.json",
        key: "file",
        skipped: &["-401", "-copy"],
        readers: &["tests/common/media.rs"],
    },
    // `Recorded::new`.
    Iterated {
        index: "sources/open_food_facts/provenance.json",
        key: "file",
        skipped: &[],
        readers: &["tests/open_food_facts.rs"],
    },
    // `recorded_public_adapter_responses_replay_without_network`.
    Iterated {
        index: "adapters/responses.json",
        key: "path",
        skipped: &[],
        readers: &["tests/adapters.rs"],
    },
];

/// Names a reader builds from a request or a list: `{prefix}{token}{suffix}`, the token being a
/// part the reader's code spells (an id, a stem, an entity).
struct Family {
    /// Under `tests/fixtures`; files in its subdirectories count too.
    dir: &'static str,
    /// Tried in order: put a longer prefix before one it starts with.
    prefixes: &'static [&'static str],
    suffixes: &'static [&'static str],
    token: fn(&str) -> bool,
    readers: &'static [&'static str],
}
fn any(_: &str) -> bool {
    true
}
fn digits(t: &str) -> bool {
    t.bytes().all(|b| b.is_ascii_digit())
}
fn item(t: &str) -> bool {
    t.strip_prefix('Q').is_some_and(digits)
}
const FAMILIES: &[Family] = &[
    // `fixture(entity)`, `{entity}.jsonl` for each of `ENTITIES`.
    Family {
        dir: "catalogue/musicbrainz",
        prefixes: &[""],
        suffixes: &[".jsonl"],
        token: any,
        readers: &[
            "tests/catalogue_musicbrainz.rs",
            "src/catalogue/musicbrainz.rs",
        ],
    },
    // Wikidata's read-back of an item, by its Q-id.
    Family {
        dir: "commons_licences",
        prefixes: &["wd-entity-"],
        suffixes: &[".json"],
        token: item,
        readers: &["tests/commons_licences.rs"],
    },
    // `site()`: each date's page and calendar file under its number.
    Family {
        dir: "events/meetup",
        prefixes: &[""],
        suffixes: &[".html", ".ics"],
        token: digits,
        readers: &["tests/meetup_series.rs"],
    },
    // `fixture(name)`: `{name}.uri` and `{name}.json`.
    Family {
        dir: "pubky_apps",
        prefixes: &[""],
        suffixes: &[".json", ".uri"],
        token: any,
        readers: &["tests/pubky_apps.rs"],
    },
    // `item(id)`: `hn-item-{id}.json`.
    Family {
        dir: "sources/hacker_news",
        prefixes: &["hn-item-"],
        suffixes: &[".json"],
        token: digits,
        readers: &["src/sources/hacker_news.rs"],
    },
    // `card(name, id)`: `{name}.json`.
    Family {
        dir: "sources/internet_archive",
        prefixes: &[""],
        suffixes: &[".json"],
        token: any,
        readers: &["src/sources/internet_archive.rs"],
    },
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Rust source's string literals (`".."`, `b".."`, `r#".."#`, `br".."`; a line continuation
/// dropped, other escapes kept as written), and the source with its comments blanked.
fn lex(src: &str) -> (Vec<String>, String) {
    let b = src.as_bytes();
    let (mut literals, mut code, mut i) = (Vec::new(), String::new(), 0);
    let after_ident = |i: usize| i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
    while i < b.len() {
        let rest = &src[i..];
        let prefix = if after_ident(i) {
            None
        } else if rest.starts_with("br") {
            Some(2)
        } else if rest.starts_with('r') {
            Some(1)
        } else {
            None
        };
        let raw = prefix.and_then(|n| {
            let hashes = rest[n..].bytes().take_while(|c| *c == b'#').count();
            (rest.as_bytes().get(n + hashes) == Some(&b'"')).then_some((n, hashes))
        });
        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
            code.push(' ');
        } else if rest.starts_with("/*") {
            let mut depth = 0;
            loop {
                if src[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if src[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += src[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            code.push(' ');
        } else if let Some((n, hashes)) = raw {
            let start = i + n + hashes + 1;
            let close = format!("\"{}", "#".repeat(hashes));
            let end = start
                + src[start..]
                    .find(&close)
                    .expect("an unterminated raw string");
            literals.push(src[start..end].to_owned());
            code.push_str(&src[i..end + close.len()]);
            i = end + close.len();
        } else if rest.starts_with('"') || (rest.starts_with("b\"") && !after_ident(i)) {
            let start = i + if rest.starts_with('"') { 1 } else { 2 };
            let mut j = start;
            while b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let mut text = String::new();
            let mut lines = src[start..j].split("\\\n");
            text.push_str(lines.next().unwrap_or_default());
            for line in lines {
                text.push_str(line.trim_start());
            }
            literals.push(text);
            code.push_str(&src[i..=j]);
            i = j + 1;
        } else if let Some(after) = rest.strip_prefix('\'') {
            // A char literal ('x', '\'', '\u{2014}'), or a lifetime ('a).
            let step = if after.starts_with('\\') {
                3 + rest[3..].find('\'').unwrap() + 1
            } else {
                let c = after.chars().next().map_or(0, char::len_utf8);
                if after[c..].starts_with('\'') {
                    c + 2
                } else {
                    1
                }
            };
            code.push_str(&rest[..step]);
            i += step;
        } else {
            let c = rest.chars().next().unwrap();
            code.push(c);
            i += c.len_utf8();
        }
    }
    (literals, code)
}

#[test]
fn the_lexer_finds_literals_and_drops_comments() {
    let (literals, code) = lex(concat!(
        "let a = r#\"x \"y\"\"#; let b = b\"z\"; let c = '\\''; let d = 'é';\n",
        "fn f<'a>(s: &'a str) {} // \"no\"\n",
        "/* \"no\" /* nested */ */ let e = \"p\\\"q\"; let r#type = \"long \\\n    line\";",
    ));
    assert_eq!(literals, ["x \"y\"", "z", "p\\\"q", "long line"]);
    assert!(!code.contains("no") && code.contains("fn f<'a>"));
}

#[test]
fn every_fixture_is_read_by_a_test() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = root.join("tests/fixtures");
    let rel = |p: &Path, base: &Path| {
        p.strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };

    // The crate's code: its literals, and each file's code without comments.
    let mut sources = vec![];
    for dir in ["src", "tests", "examples", "benches"] {
        if root.join(dir).is_dir() {
            walk(&root.join(dir), &mut sources);
        }
    }
    let mut literals = BTreeSet::new();
    // Each code file's stem and literals, for a file name several fixture directories hold.
    let mut file_literals: Vec<(String, BTreeSet<String>)> = vec![];
    let mut code = BTreeMap::new();
    for path in sources {
        let name = rel(&path, root);
        if path.starts_with(&fixtures)
            || path.extension().is_none_or(|e| e != "rs")
            || name == "tests/fixture_orphans.rs"
        {
            continue;
        }
        let (found, text) = lex(&fs::read_to_string(&path).unwrap());
        literals.extend(found.iter().cloned());
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        file_literals.push((stem, found.into_iter().collect()));
        code.insert(name, text);
    }
    // A literal names a path under tests/fixtures whole, or as the end of a longer path.
    let mut ends = BTreeSet::new();
    for l in &literals {
        ends.insert(l.as_str());
        ends.extend(l.match_indices('/').map(|(at, _)| &l[at + 1..]));
    }

    let mut files = vec![];
    walk(&fixtures, &mut files);
    let files: BTreeSet<String> = files.iter().map(|f| rel(f, &fixtures)).collect();
    // A file name in several directories is named by its bare name only in a code file named
    // after one of the file's directories (`sources/artic/artwork-missing.json` by
    // `src/sources/artic.rs`), as a source's own tests read its recordings: an unread copy is not
    // read because another directory's twin is.
    let mut copies: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &files {
        *copies.entry(file.rsplit('/').next().unwrap()).or_default() += 1;
    }
    let mut read = BTreeSet::new();
    for file in &files {
        let (dir, name) = file.rsplit_once('/').unwrap_or(("", file));
        let by_name = if copies[name] > 1 {
            file_literals
                .iter()
                .any(|(stem, lits)| lits.contains(name) && dir.split('/').any(|d| d == stem))
        } else {
            literals.contains(name)
        };
        if EXEMPT.contains(&name)
            || PARSER_INPUT.contains(&file.as_str())
            || by_name
            || ends.contains(file.as_str())
        {
            read.insert(file.clone());
        }
    }

    for it in ITERATED {
        let index_name = it.index.rsplit('/').next().unwrap();
        for name in it.readers {
            let Some(text) = reader(&code, name) else {
                continue;
            };
            assert!(
                text.contains(index_name),
                "{name} no longer reads {}",
                it.index
            );
        }
        let indexes: Vec<String> = match it.index.split_once("/*/") {
            Some((parent, leaf)) => files
                .iter()
                .filter(|f| {
                    f.strip_prefix(&format!("{parent}/"))
                        .and_then(|r| r.strip_suffix(&format!("/{leaf}")))
                        .is_some_and(|d| !d.contains('/'))
                })
                .cloned()
                .collect(),
            None => vec![it.index.to_owned()]
                .into_iter()
                .filter(|i| files.contains(i))
                .collect(),
        };
        assert!(!indexes.is_empty() || removing(), "no {}", it.index);
        for index in indexes {
            let dir = index.rsplit_once('/').map_or("", |(d, _)| d);
            let raw = fs::read(fixtures.join(&index)).unwrap();
            let rows: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            let rows: Vec<(Option<&str>, &serde_json::Value)> = match &rows {
                serde_json::Value::Object(map) => {
                    map.iter().map(|(k, v)| (Some(k.as_str()), v)).collect()
                }
                serde_json::Value::Array(list) => list.iter().map(|v| (None, v)).collect(),
                _ => panic!("{index}: neither an object nor a list"),
            };
            for (key, row) in rows {
                let Some(file) = row[it.key].as_str() else {
                    continue;
                };
                let skipped = key.is_some_and(|k| it.skipped.iter().any(|s| k.ends_with(s)));
                if !skipped || key.is_some_and(|k| literals.contains(k)) {
                    read.insert(format!("{dir}/{file}"));
                }
            }
        }
    }

    for family in FAMILIES {
        let text = family
            .readers
            .iter()
            .filter_map(|r| reader(&code, r))
            .collect::<Vec<_>>()
            .join("\n");
        for file in files
            .iter()
            .filter(|f| f.starts_with(&format!("{}/", family.dir)))
        {
            let name = file.rsplit('/').next().unwrap();
            let token = family
                .prefixes
                .iter()
                .find_map(|p| name.strip_prefix(p))
                .and_then(|rest| family.suffixes.iter().find_map(|s| rest.strip_suffix(s)));
            if token.is_some_and(|t| !t.is_empty() && (family.token)(t) && text.contains(t)) {
                read.insert(file.clone());
            }
        }
    }

    let orphans: Vec<&String> = files.difference(&read).collect();
    assert!(
        orphans.is_empty(),
        "fixtures no test reads: delete each with its provenance row, or write the test it was \
         recorded for (a reader that builds names goes in FAMILIES, an index a test walks in \
         ITERATED):\n{}",
        orphans
            .iter()
            .map(|o| format!("  tests/fixtures/{o}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
