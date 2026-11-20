//! The parse job's side of the worker's protocol, against a worker that misbehaves. A process
//! has one worker (`html::set_worker` counts once), so these tests have a file of their own:
//! their worker is a shell script that does what the file named in its first argument says.
//! They need `/bin/sh` and `/bin/sleep`.
use pinakes_core::resolver::html;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The worker: `sh -c STUB pinakes-parse-stub <mode file> <pinakes-parse> <cpu seconds>`.
/// An answer starts with a status byte and the tree's length in four bytes, low byte first.
/// - `parse`: the real worker in its place.
/// - `long-answer`: the head of an answer of `u32::MAX` bytes, then it waits for the end of its
///   input.
/// - `half-answer`: the head of an answer of 100 bytes and four of them, then it ends.
/// - `no-answer`: it ends at once.
/// - `asleep`: it sleeps for thirty seconds.
/// - `deaf`: the head of an answer of `u32::MAX` bytes, then it sleeps for a minute whatever
///   happens to its input.
/// - `aborted`: it dies of `SIGABRT`, as a worker does when an allocation fails.
const STUB: &str = r#"read -r mode < "$1"
case "$mode" in
parse) exec "$2" "$3" ;;
long-answer)
    printf '\000\377\377\377\377'
    while read -r line; do :; done
    ;;
half-answer) printf '\000\144\000\000\000half' ;;
no-answer) ;;
asleep) exec /bin/sleep 30 ;;
deaf)
    printf '\000\377\377\377\377'
    exec /bin/sleep 60
    ;;
aborted)
    ulimit -c 0
    kill -s ABRT "$$"
    ;;
esac
"#;
const PAGE: &str = "<title>Kept</title><p>x</p>";

/// One test at a time: they share the worker's mode file.
static SERIAL: Mutex<()> = Mutex::new(());
fn mode_file() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pinakes-parse-stub-{}", std::process::id()))
}
/// The stub in one mode, for as long as it is held.
struct Stub {
    _serial: MutexGuard<'static, ()>,
}
impl Drop for Stub {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(mode_file());
    }
}
fn stub(mode: &str) -> Stub {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::write(mode_file(), mode).unwrap();
    html::set_worker(
        "/bin/sh",
        [
            std::ffi::OsString::from("-c"),
            STUB.into(),
            "pinakes-parse-stub".into(),
            mode_file().into_os_string(),
            env!("CARGO_BIN_EXE_pinakes-parse").into(),
        ],
    );
    assert!(html::has_worker());
    Stub { _serial: serial }
}
/// What a parse of `PAGE` through the worker fails with, as a job of its own with `budget` of
/// CPU time: the reason, and the failure kind the queue stores for it.
fn failure(budget: Duration) -> (&'static str, &'static str) {
    let error = html::document_unscanned(PAGE, budget).unwrap_err();
    let class = pinakes_core::resolver::error_class(&error);
    let parse = error
        .downcast_ref::<html::TooComplex>()
        .unwrap_or_else(|| panic!("{error:#}"));
    (parse.reason, class)
}

/// The stub parses when it is told to: what the other tests see is what its other modes do.
#[test]
fn the_stub_parses_when_told_to() {
    let _stub = stub("parse");
    let html = html::document_unscanned(PAGE, Duration::from_secs(1)).unwrap();
    let title = scraper::Selector::parse("title").unwrap();
    assert_eq!(
        html.select(&title)
            .next()
            .map(|t| t.text().collect::<String>())
            .as_deref(),
        Some("Kept")
    );
}

/// An answer whose head says it is longer than a tree may be is refused at its head: no room is
/// made for it and none of it is read. The stub waits for the end of its input after the head,
/// so a job that waited for the answer's bytes would wait until its wall deadline; this one ends
/// at once, as a worker that ended early does.
#[test]
fn an_answer_longer_than_the_bound_is_refused_at_its_head() {
    let _stub = stub("long-answer");
    assert_eq!(
        failure(Duration::from_secs(1)),
        ("the worker ended early", "timeout")
    );
}

/// A worker that ends in the middle of its answer, or before any, ended early: a timeout, so
/// the card stays and the page is read again.
#[test]
fn a_worker_that_ends_mid_answer_ended_early() {
    for mode in ["half-answer", "no-answer"] {
        let _stub = stub(mode);
        assert_eq!(
            failure(Duration::from_secs(1)),
            ("the worker ended early", "timeout"),
            "{mode}"
        );
    }
}

/// A worker still running at its job's wall deadline is killed there. The deadline follows from
/// the job's budget (three times it and a second: one second here); the stub would sleep thirty.
#[test]
fn a_worker_asleep_at_the_wall_deadline_is_killed() {
    let _stub = stub("asleep");
    let started = Instant::now();
    assert_eq!(
        failure(Duration::from_millis(1)),
        ("the wall deadline", "timeout")
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(20),
        "{elapsed:?}"
    );
}

/// A worker that goes on after its answer was refused, whatever happens to its input, is killed
/// when the job ends: the job does not wait for it. The stub would sleep for a minute.
#[test]
fn a_worker_deaf_to_the_end_of_its_input_is_killed() {
    let _stub = stub("deaf");
    let started = Instant::now();
    assert_eq!(
        failure(Duration::from_secs(1)),
        ("the worker ended early", "timeout")
    );
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
}

/// A worker dead of `SIGABRT` died at its memory limit (`parse_worker` shows the real worker
/// does, asked for more than the limit): `too_complex`, no card and no retry.
#[test]
fn a_worker_that_aborts_died_at_its_memory_limit() {
    let _stub = stub("aborted");
    assert_eq!(
        failure(Duration::from_secs(1)),
        ("the memory limit", "too_complex")
    );
}
