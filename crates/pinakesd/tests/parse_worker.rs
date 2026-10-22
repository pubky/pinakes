//! The daemon is its own HTML parse worker: `pinakesd parse-worker SECS` parses what stdin holds
//! under the worker's limits, and the daemon's startup check passes with it.
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn the_daemon_parses_as_its_own_worker() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pinakesd"))
        .args(["parse-worker", "10"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\x00<p>worker</p>")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    assert!(out.stdout.len() > 5, "an answer with a tree");
    pinakes_core::resolver::html::set_worker(env!("CARGO_BIN_EXE_pinakesd"), ["parse-worker"]);
    pinakes_core::resolver::html::check_worker().unwrap();
}
