//! What this crate's tests share with its integration tests (feature `test-util`, which only the
//! crate's own dev-dependency turns on): never part of the daemon's build.
//!
//! A test that needs a source asks for it first, `if !needs_also_on(&["deezer"]) { return; }`.
//! The answer is `true` when this build registers every name. A name it does not register is a
//! mistake (a typo, a rename, a source removed without the tests that need it) and panics, so a
//! test never passes without what it tests. The one exception is `scripts/check-removals`, which
//! runs the tests on a scratch copy without one source and names it in [`REMOVING`]: then a test
//! that needs what the removal took returns early, as it would be deleted with the source.
//!
//! A case of a test's loop that needs a source asks `needs_*` with the case's name and skips the
//! case only during a removal. A part of a test that runs only when a source is there (an extra
//! check) asks `has_sources` or `has_also_on`, which never panic; a configuration helper sets a
//! source's switch only after `switch_also_on`, which fails on a misspelled name. Every call's
//! names, when they are one literal list, are checked against the registries by
//! `guards_name_registered_sources`, so a misspelled name fails there too; every `needs_*` or
//! `switch_also_on` call that goes without a name during a removal writes a line to
//! [`REMOVING_LOG`], whatever form its names take.

/// The variable `scripts/check-removals` sets to the source it removes while it runs the tests.
pub const REMOVING: &str = "PINAKES_REMOVING";
/// A file `scripts/check-removals` names with [`REMOVING`]: each test that returns early for the
/// removal adds a line (the test, what it needs), which the script lists after the run.
pub const REMOVING_LOG: &str = "PINAKES_REMOVING_LOG";

/// Whether this build registers each of these official sources ([`crate::sources::by_name`]).
pub fn needs_sources(names: &[&str]) -> bool {
    needs("official source", names, source)
}

/// Whether this build registers each of these also-on sources (their [`crate::also_on::Source::id`]).
pub fn needs_also_on(ids: &[&str]) -> bool {
    needs("also-on source", ids, also_on)
}

/// Whether this build registers each of these lookup families ([`crate::lookups::FAMILIES`]).
pub fn needs_families(names: &[&str]) -> bool {
    needs("lookup family", names, family)
}

/// Whether some source of this build gives each of these attributes
/// ([`crate::sources::attributes`]).
pub fn needs_attributes(names: &[&str]) -> bool {
    needs("attribute", names, attribute)
}

/// Whether a configuration helper may set a switch for each of these also-on sources (it sets
/// one only for a source the build has, on or off as the test wants): `true` when this build
/// registers them, a panic on a name it does not register (a misspelled switch would be dropped
/// unseen), `false` only while a removal takes it.
pub fn switch_also_on(ids: &[&str]) -> bool {
    needs("switch of also-on source", ids, also_on)
}

/// Whether this build registers each of these official sources, for a part of a test that runs
/// only with them; never panics.
pub fn has_sources(names: &[&str]) -> bool {
    names.iter().all(|n| source(n))
}

/// Whether this build registers each of these also-on sources, for a part of a test that runs
/// only with them; never panics.
pub fn has_also_on(ids: &[&str]) -> bool {
    ids.iter().all(|id| also_on(id))
}

/// What a test needs that is not a name: the first registered thing of a kind (`what` says
/// which, as "an official source that takes a token"). `None` only while a removal takes the last
/// one; otherwise it panics.
pub fn needs_one<T>(found: Option<T>, what: &str) -> Option<T> {
    if found.is_none() {
        missing(what);
    }
    found
}

fn source(name: &str) -> bool {
    crate::sources::by_name(name).is_some()
}
fn also_on(id: &str) -> bool {
    crate::also_on::registered().iter().any(|s| s.id() == id)
}
fn family(name: &str) -> bool {
    crate::lookups::FAMILIES.iter().any(|f| f.name() == name)
}
fn attribute(name: &str) -> bool {
    crate::sources::attributes().contains(&name)
}

fn needs(kind: &str, names: &[&str], registered: fn(&str) -> bool) -> bool {
    let gone: Vec<&str> = names.iter().copied().filter(|n| !registered(n)).collect();
    if gone.is_empty() {
        return true;
    }
    missing(&format!("{kind} {gone:?}"));
    false
}

/// Returns while a removal is checked (the test then returns early); panics otherwise.
fn missing(what: &str) {
    match std::env::var(REMOVING) {
        Ok(removing) if !removing.is_empty() => {
            let test = std::thread::current().name().unwrap_or("?").to_owned();
            let line = format!("{test}: without {what}, which the removal of {removing} takes\n");
            eprint!("{line}");
            if let Ok(log) = std::env::var(REMOVING_LOG) {
                use std::io::Write;
                // One write of the whole line: tests run on several threads.
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                    .and_then(|mut file| file.write_all(line.as_bytes()))
                    .expect("the removal's log");
            }
        }
        _ => panic!(
            "this build registers no {what}: fix the name, or delete the test with its source \
             (AGENTS.md, Removing a source)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// Whether a registry has a name.
    type Registered = fn(&str) -> bool;
    /// The calls that name sources, each with the registry its names are in.
    const GUARDS: &[(&str, Registered)] = &[
        ("needs_sources(", super::source),
        ("needs_also_on(", super::also_on),
        ("needs_families(", super::family),
        ("needs_attributes(", super::attribute),
        ("has_sources(", super::source),
        ("has_also_on(", super::also_on),
        ("switch_also_on(", super::also_on),
    ];

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "fixtures") {
                    rust_files(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Every Rust file of this crate's tests and sources, with its text, except this one.
    fn sources() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src", "tests", "examples"] {
            if root.join(dir).is_dir() {
                rust_files(&root.join(dir), &mut files);
            }
        }
        files.sort();
        files
            .into_iter()
            .filter(|p| !p.ends_with("src/test_support.rs"))
            .map(|p| {
                let text = std::fs::read_to_string(&p).unwrap();
                let name = p.strip_prefix(root).unwrap().display().to_string();
                (name, text)
            })
            .collect()
    }

    /// The test code of a file: all of an integration test's, a source file's test module.
    fn test_code<'a>(file: &str, text: &'a str) -> Option<(usize, &'a str)> {
        if !file.starts_with("src/") {
            return Some((0, text));
        }
        let at = text.find("#[cfg(test)]\nmod ")?;
        Some((at, &text[at..]))
    }

    fn line(text: &str, at: usize) -> usize {
        text[..at].matches('\n').count() + 1
    }

    /// The quoted names of a call's `&[…]` argument, or `None` when it is not one literal list.
    fn names(argument: &str) -> Option<Vec<&str>> {
        let list = argument.trim_start().strip_prefix("&[")?;
        let list = &list[..list.find(']')?];
        let mut names = Vec::new();
        for item in list.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            names.push(item.strip_prefix('"')?.strip_suffix('"')?);
        }
        (!names.is_empty()).then_some(names)
    }

    /// Every literal name a test asks for is registered: a guard whose name is a typo or a renamed
    /// source would otherwise fail only in a test that runs, and an optional part (`has_*`) would
    /// never run. A list that is not literal (a loop's ids) is checked when it runs. While
    /// `scripts/check-removals` checks a removal, the names that removal took are expected to be
    /// gone.
    #[test]
    fn guards_name_registered_sources() {
        let removing = std::env::var(super::REMOVING).is_ok_and(|r| !r.is_empty());
        let mut wrong = Vec::new();
        let mut seen = 0;
        for (file, text) in sources() {
            for (guard, registered) in GUARDS {
                for (at, _) in text.match_indices(guard) {
                    // A definition, or a call of another function whose name ends the same.
                    let before = text[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                        || text[..at].ends_with("fn ")
                    {
                        continue;
                    }
                    let Some(names) = names(&text[at + guard.len()..]) else {
                        continue;
                    };
                    seen += 1;
                    for name in names {
                        if !registered(name) && !removing {
                            wrong.push(format!(
                                "{file}:{}: {guard}…) names {name:?}, which this build does not \
                                 register",
                                line(&text, at)
                            ));
                        }
                    }
                }
            }
        }
        assert!(seen > 50, "the guards are found ({seen})");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// What a registry check in a test reads: a test that asks a registry itself and returns
    /// without the source would pass without what it tests.
    const REGISTRY_READS: &[&str] = &[
        "registry()",
        "registered()",
        "REGISTRY",
        "FAMILIES",
        "by_name(",
        "attributes()",
    ];

    /// `text` up to `n` bytes at most, cut at a character boundary.
    fn upto(text: &str, n: usize) -> &str {
        let mut n = n.min(text.len());
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        &text[..n]
    }

    /// The `if` or `let … else` statement whose block starts at `at` (`{`), when the block starts
    /// with a bare `return` or `continue`: (its start, its head, whether it returns). The head
    /// starts after the `;`, `{` or `}` that ends the statement before, at the same brace depth,
    /// so a closure's block inside the head (`find_map(|s| { … })`) is part of it.
    fn skip_head(text: &str, at: usize) -> Option<(usize, &str, bool)> {
        let body = text[at + 1..].trim_start();
        let returns = body.starts_with("return;") || body.starts_with("return Ok(());");
        if !returns && !body.starts_with("continue;") {
            return None;
        }
        let mut depth = 0usize;
        let mut start = 0;
        for (i, c) in text[..at].char_indices().rev() {
            match c {
                '}' => depth += 1,
                '{' if depth > 0 => depth -= 1,
                '{' | ';' if depth == 0 => {
                    start = i + 1;
                    break;
                }
                _ => {}
            }
        }
        let head = text[start..at].trim_start();
        (head.starts_with("if ") || head.starts_with("let "))
            .then(|| (at - head.len(), head, returns))
    }

    /// The silent skips of one file's tests (`tests_ask_for_sources_through_needs`): each one's
    /// byte offset in `text` and what it is.
    fn silent_skips(file: &str, text: &str) -> Vec<(usize, String)> {
        let mut silent = Vec::new();
        let Some((start, code)) = test_code(file, text) else {
            return silent;
        };
        for (at, _) in code.match_indices('{') {
            let Some((head_at, head, returns)) = skip_head(code, at) else {
                continue;
            };
            let own_check =
                REGISTRY_READS.iter().any(|r| head.contains(r)) && !head.contains("needs_");
            let optional = returns && head.contains("has_") && !head.contains("needs_");
            if own_check || optional {
                let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
                silent.push((start + head_at, head));
            }
        }
        let helpers = code
            .match_indices("-> bool {")
            .chain(code.match_indices("-> Option<"));
        for (at, _) in helpers {
            let body = &code[at..];
            let body = upto(body, body.find("\n}\n").unwrap_or(body.len()).min(600));
            if REGISTRY_READS.iter().any(|r| body.contains(r)) && !body.contains("needs_") {
                silent.push((
                    start + at,
                    "a helper answers whether a source is registered".into(),
                ));
            }
        }
        silent
    }

    /// No test skips itself, or a case of its loop, on a registry check of its own: it asks
    /// `needs_*`, which fails on a name this build does not register. A whole test never returns
    /// on `has_*` either (that is for optional parts), and no test helper answers whether a source
    /// is registered (the old `fn has(…) -> bool`) or gives `None` without one, other than through
    /// `needs_*`.
    #[test]
    fn tests_ask_for_sources_through_needs() {
        let mut silent = Vec::new();
        for (file, text) in sources() {
            for (at, what) in silent_skips(&file, &text) {
                silent.push(format!("{file}:{}: {what}", line(&text, at)));
            }
        }
        assert!(
            silent.is_empty(),
            "a test skips on a registry check of its own; use test_support::needs_*:\n{}",
            silent.join("\n")
        );
    }

    /// Each silent form is found, and none of the forms that ask `needs_*` or skip an optional
    /// case is.
    #[test]
    fn the_skip_lint_finds_each_silent_form() {
        let silent = [
            "if by_name(\"x\").is_none() {\n    return;\n}",
            "let Some(x) = by_name(\"x\") else {\n    return;\n};",
            "let Some(s) = REGISTRY.iter().find_map(|s| {\n    Some(s)\n}) else {\n    return;\n};",
            "if !REGISTRY.iter().any(|s| {\n    s.name() == \"x\"\n}) {\n    return;\n}",
            "for s in [\"x\"] {\n    if by_name(s).is_none() {\n        continue;\n    }\n}",
            "if !has_also_on(&[\"x\"]) {\n    return;\n}",
            "fn has(id: &str) -> bool {\n    registered().iter().any(|s| s.id() == id)\n}\n",
            "fn first() -> Option<&'static str> {\n    REGISTRY.first().map(|s| s.name())\n}\n",
        ];
        for form in silent {
            assert_eq!(silent_skips("tests/form.rs", form).len(), 1, "{form}");
        }
        let asked = [
            "if !needs_sources(&[\"x\"]) {\n    return;\n}",
            "let Some(s) = needs_one(REGISTRY.iter().find_map(|s| {\n    Some(s)\n}), \"one\") else {\n    return;\n};",
            "for s in [\"x\"] {\n    if !has_also_on(&[s]) {\n        continue;\n    }\n}",
            "fn first() -> Option<&'static str> {\n    needs_one(REGISTRY.first().map(|s| s.name()), \"one\")\n}\n",
            "if by_name(\"x\").is_none() {\n    return;\n}",
        ];
        for (n, form) in asked.iter().enumerate() {
            // The last is production code: the lint reads a source file's test module only.
            let file = if n == asked.len() - 1 {
                "src/form.rs"
            } else {
                "tests/form.rs"
            };
            assert!(silent_skips(file, form).is_empty(), "{form}");
        }
        // A multi-byte character at the helper body's cut is no panic.
        let long = format!("fn h() -> bool {{\n    // {}\n}}\n", "·".repeat(400));
        assert!(silent_skips("tests/form.rs", &long).is_empty());
    }

    #[test]
    fn a_missing_name_panics_and_a_removal_skips() {
        assert!(super::needs_sources(&[]));
        assert!(!super::has_sources(&["no_such_source"]));
        // REMOVING is unset in a normal run (check-removals sets it for its scratch runs alone).
        if !std::env::var(super::REMOVING).is_ok_and(|r| !r.is_empty()) {
            let gone = std::panic::catch_unwind(|| super::needs_sources(&["no_such_source"]));
            assert!(gone.is_err(), "an unregistered name panics");
        }
    }
}
