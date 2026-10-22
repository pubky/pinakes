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

/// An Internet Archive answer with 20,000 descriptions in markup (Codex's case): one job, one
/// worker for all of them, done well within the job's limits. One worker per fragment took
/// minutes and outlived its caller.
#[test]
fn many_fragments_in_one_job_share_one_worker() {
    let _serial = worker();
    let descriptions: Vec<String> = (0..20_000).map(|n| format!("<b>x{n}</b>")).collect();
    let body = serde_json::json!({
        "metadata": {"identifier": "many", "title": "Many", "description": descriptions}
    })
    .to_string();
    let started = Instant::now();
    let card = html::off_runtime(|| {
        pinakes_core::sources::internet_archive::parse(200, body.as_bytes(), "many")
    })
    .unwrap()
    .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
    let description = card.description.unwrap();
    assert!(
        description.starts_with("x0\nx1\n"),
        "{}",
        &description[..20]
    );
    assert!(description.ends_with("x19999"));
    assert!(no_worker_left());
}

/// A job whose caller stops waiting ends at once: its worker is killed, every later parse of
/// the job fails, markup and plain text alike, and the slot is free well before the loop would
/// have ended.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_cancelled_job_stops_its_loop() {
    let _serial = worker();
    for (name, fragment) in [("markup", "<b>x{n}</b>"), ("plain text", "x{n} plain")] {
        let before = html::running();
        let refused = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let parsed_after = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (counted, after, cancelled) = (refused.clone(), parsed_after.clone(), gone.clone());
        let job = tokio::spawn(html::blocking(move || {
            for n in 0..5_000_000 {
                let text = fragment.replace("{n}", &n.to_string());
                match html::fragment(&text) {
                    Err(_) => {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(_) if cancelled.load(std::sync::atomic::Ordering::SeqCst) => {
                        after.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(_) => {}
                }
            }
        }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(html::running() > before, "{name}");
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        job.abort();
        let started = Instant::now();
        while html::running() != before && started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(html::running(), before, "{name}: {:?}", started.elapsed());
        assert!(
            refused.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "{name}"
        );
        // The test marks the caller gone just before `abort`, and the job learns it when the
        // runtime drops the future: a few parses fit between the two, not the hundreds of
        // thousands that went on before (Codex counted 944,000 in 5.26 s).
        assert!(
            parsed_after.load(std::sync::atomic::Ordering::SeqCst) <= 1_000,
            "{name}: {} parses after the caller left",
            parsed_after.load(std::sync::atomic::Ordering::SeqCst)
        );
    }
    assert!(no_worker_left());
}

/// An Internet Archive answer of 250,000 plain descriptions (about 1 MB, Codex's case): its
/// caller gone, the job refuses the rest of them and frees its slot within a fraction of a
/// second, instead of parsing on.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_cancelled_job_refuses_the_rest_of_a_plain_answer() {
    let _serial = worker();
    let descriptions: Vec<String> = (0..250_000).map(|n| format!("plain {n}")).collect();
    let body = serde_json::json!({
        "metadata": {"identifier": "many", "title": "Many", "description": descriptions}
    })
    .to_string();
    assert!(body.len() > 1_000_000);
    let before = html::running();
    let job = tokio::spawn(html::blocking(move || {
        pinakes_core::sources::internet_archive::parse(200, body.as_bytes(), "many").map(|_| ())
    }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    job.abort();
    let started = Instant::now();
    while html::running() != before && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(html::running(), before);
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "{:?}",
        started.elapsed()
    );
}

/// A 1,000-event calendar as calendars are written: plain locations and descriptions with a
/// meeting link, and an HTML description per event. It parses within the calendar's own budget
/// (two seconds by default) with every fragment parse through the worker, and its conference
/// links are found.
#[test]
fn a_thousand_event_calendar_parses_in_its_budget_with_the_worker() {
    let _serial = worker();
    let mut ics = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n");
    for n in 0..1_000 {
        ics.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:event-{n}@example.org\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:20261101T{h:02}0000Z\r\nDTEND:20261101T{h:02}3000Z\r\nSUMMARY:Meeting {n}\r\nLOCATION:Room {n}\r\nDESCRIPTION:Join at https://meet.example.org/room-{n} and bring notes.\r\nX-ALT-DESC;FMTTYPE=text/html:<p>Join <a href=\"https://zoom.us/j/{n}\">on Zoom</a></p>\r\nEND:VEVENT\r\n",
            h = n % 24
        ));
    }
    ics.push_str("END:VCALENDAR\r\n");
    assert!(ics.len() > 150_000);
    let started = Instant::now();
    let doc = html::off_runtime(|| {
        pinakes_core::event::calendar::parse(
            "https://example.org/calendar.ics",
            ics.as_bytes(),
            &Default::default(),
            chrono::Utc::now(),
        )
    })
    .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    let container = doc.container.expect("a calendar");
    assert_eq!(container.occurrences.len(), 1_000);
    assert!(no_worker_left());
}
