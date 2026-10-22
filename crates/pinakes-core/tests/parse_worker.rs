//! The HTML parse worker: these tests parse in `pinakes-parse`, the worker cargo builds with
//! them, under its limits.
use pinakes_core::resolver::html;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One test at a time: `no_worker_left` looks at every child of this process.
static SERIAL: Mutex<()> = Mutex::new(());
fn worker() -> MutexGuard<'static, ()> {
    html::set_worker(env!("CARGO_BIN_EXE_pinakes-parse"), Vec::<&str>::new());
    assert!(html::has_worker());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
fn title(html: &scraper::Html) -> Option<String> {
    let title = scraper::Selector::parse("title").unwrap();
    html.select(&title).next().map(|t| t.text().collect())
}
/// Codex's shapes, each with a title before it: misnested `<span><div></span>`, a tag inside
/// SVG's `<title>` followed by 20,000 `<div>`, 60,000 attributes on an end tag and on a start
/// tag, 20,000 unclosed `<div>`, 30,000 `<span>x`.
fn shapes() -> Vec<(&'static str, String)> {
    let attributes = |tag: &str| {
        let mut text = format!("<title>Kept</title>{tag}");
        for n in 0..60_000 {
            text.push_str(&format!(" a{n}='x'"));
        }
        text.push('>');
        text
    };
    vec![
        (
            "misnested",
            format!("<title>Kept</title>{}", "<span><div></span>".repeat(20_000)),
        ),
        (
            "svg title",
            format!("<title>Kept</title><svg><title>{}", "<div>".repeat(20_000)),
        ),
        ("end tag attributes", attributes("</div")),
        ("start tag attributes", attributes("<div")),
        (
            "unclosed",
            format!("<title>Kept</title>{}", "<div>".repeat(20_000)),
        ),
        (
            "spans",
            format!("<title>Kept</title>{}", "<span>x".repeat(30_000)),
        ),
    ]
}
/// No `pinakes-parse` process of ours is left.
fn no_worker_left() -> bool {
    let mine = std::process::id();
    std::fs::read_dir("/proc").unwrap().all(|e| {
        let e = e.unwrap();
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            return true;
        };
        if pid == mine {
            return true;
        }
        let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        // Our children only: the parent pid is the fourth field.
        let parent = stat
            .rsplit(") ")
            .next()
            .and_then(|rest| rest.split(' ').nth(1));
        parent != Some(&mine.to_string())
            || !std::fs::read(e.path().join("cmdline"))
                .unwrap_or_default()
                .windows(13)
                .any(|w| w == b"pinakes-parse")
    })
}

/// Each shape ends at once: a card with its title when the scan cuts it, `too_complex` when
/// the worker dies at its limits. No worker survives.
#[test]
fn codex_s_shapes_end_at_once_with_a_card_or_too_complex() {
    let _serial = worker();
    for (name, page) in shapes() {
        let started = Instant::now();
        let out = html::document(&page);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name}: {:?}",
            started.elapsed()
        );
        match out {
            Ok(html) => assert_eq!(title(&html).as_deref(), Some("Kept"), "{name}"),
            Err(error) => assert!(error.is::<html::TooComplex>(), "{name}: {error:#}"),
        }
    }
    assert!(no_worker_left());
}

/// Without the scan, the shapes the parser works on for minutes die at the worker's CPU budget
/// (one second here), each as `too_complex`, within the wall deadline. No worker survives.
#[test]
fn an_unscanned_hostile_page_dies_at_its_budget() {
    let _serial = worker();
    for (name, page) in shapes().into_iter().take(4) {
        let started = Instant::now();
        let error = html::document_unscanned(&page, Duration::from_secs(1)).unwrap_err();
        assert!(error.is::<html::TooComplex>(), "{name}: {error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{name}: {:?}",
            started.elapsed()
        );
    }
    assert!(no_worker_left());
}

/// The tree a worker answers with is the parser's tree: every recorded page, as a document and
/// as a fragment, with its doctype and quirks mode.
#[test]
fn a_worker_parse_is_the_parser_s_tree() {
    let _serial = worker();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut stack = vec![fixtures];
    let mut pages = vec![
        "<!DOCTYPE html><html><head><title>T</title></head><body><p class=a id=b>x<br>y</p><!-- c --><?pi d?></body></html>".to_owned(),
        "<p>no doctype: quirks</p>".to_owned(),
        format!("<p>{}</p>", "é漢🙂".repeat(1_000)),
    ];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "html") {
                pages.push(std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    assert!(pages.len() > 20);
    for page in &pages {
        let plain = scraper::Html::parse_document(page);
        let ours = html::document(page).unwrap();
        assert_eq!(ours.root_element().html(), plain.root_element().html());
        assert_eq!(ours.quirks_mode, plain.quirks_mode);
        assert_eq!(
            ours.tree.nodes().filter(|n| n.value().is_doctype()).count(),
            plain
                .tree
                .nodes()
                .filter(|n| n.value().is_doctype())
                .count()
        );
        let plain = scraper::Html::parse_fragment(page);
        let ours = html::fragment(page).unwrap();
        assert_eq!(ours.root_element().html(), plain.root_element().html());
    }
}

/// A parse whose caller stopped waiting still holds its slot only until its worker dies at the
/// budget; then the slot is free again.
#[tokio::test]
// The guard only serializes this file's tests; nothing else in this test waits for it.
#[allow(clippy::await_holding_lock)]
async fn an_abandoned_parse_frees_its_slot_when_its_worker_dies() {
    let _serial = worker();
    let before = html::running();
    let (_, page) = shapes().swap_remove(3);
    let parse = tokio::spawn(html::blocking(move || {
        html::document_unscanned(&page, Duration::from_secs(1)).map(|_| ())
    }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(html::running() > before);
    parse.abort();
    let started = Instant::now();
    while html::running() != before && started.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(html::running(), before);
    assert!(no_worker_left());
}
