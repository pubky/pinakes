//! The parse worker as a program of its own: `pinakes-parse <cpu seconds>`, for the tests and
//! for a library user. The daemon is its own worker (`pinakesd parse-worker`).
fn main() {
    let secs = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    std::process::exit(pinakes_core::resolver::html::worker_main(secs));
}
