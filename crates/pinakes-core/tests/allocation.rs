//! What reading a hostile page allocates, counted on the reading thread: a cap bounds the work
//! only if nothing before it copies the whole input.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pinakes_core::event::page::performers;
use serde_json::json;
use url::Url;

struct Counting;
thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATED.try_with(|a| a.set(a.get() + layout.size()));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

/// What `f` returns and the bytes this thread allocated while it ran.
fn allocated_by<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATED.with(Cell::get);
    let out = f();
    (out, ALLOCATED.with(Cell::get) - before)
}

/// A performer's `sameAs` of 100,000 links: the 64 values read are a few kilobytes, not a copy
/// of the array (800 kB of references).
#[test]
fn a_performers_long_link_array_is_not_copied_before_its_cap() {
    let base = Url::parse("https://tickets.example/show").unwrap();
    let same_as: Vec<String> = (0..100_000)
        .map(|n| format!("https://tickets.example/fans/{n}"))
        .collect();
    let node = json!({"@type":"MusicEvent","performer":{"name":"Radiohead","sameAs":same_as}});
    let (found, bytes) = allocated_by(|| performers(&node, &base));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].links.len(), 9);
    assert!(bytes < 100_000, "{bytes} bytes allocated");
}
