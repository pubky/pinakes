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
    // One request as the job sends it: the kind, the text's length, the text.
    let text = b"<p>worker</p>";
    let mut request = vec![0u8];
    request.extend_from_slice(&(text.len() as u32).to_le_bytes());
    request.extend_from_slice(text);
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    // The answer: status 0, the tree's length, the tree.
    assert_eq!(out.stdout[0], 0, "{:?}", &out.stdout[..5]);
    let len = u32::from_le_bytes(out.stdout[1..5].try_into().unwrap()) as usize;
    assert_eq!(out.stdout.len(), 5 + len);
    assert!(len > 5, "an answer with a tree");
    pinakes_core::resolver::html::set_worker(env!("CARGO_BIN_EXE_pinakesd"), ["parse-worker"]);
    pinakes_core::resolver::html::check_worker().unwrap();
}
