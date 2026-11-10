//! Leaving a page's list entries out of its JSON-LD (`web::extract`) costs no copies: a large page
//! of nested graphs with two listed events is read within a small multiple of its size.
use pinakes_core::resolver::{egress::Fetched, web};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
mod common;

/// Counts every byte allocated (not freed) through the global allocator.
struct Counting;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

/// A page of about 900 KB: fifty nested `@graph` objects (a hundred levels of JSON, within
/// serde's limit of 128), each a page node with a long description, and two events the page
/// lists (each naming its own page), which turn the pruning on.
fn page() -> String {
    let filler = "x".repeat(18_000);
    let mut graph = String::from(r#"{"@type":"WebPage","name":"Leaf","description":"end"}"#);
    for i in 0..50 {
        graph = format!(
            r#"{{"@type":"WebPage","name":"Level {i}","description":"{filler}","@graph":[{graph}]}}"#
        );
    }
    let event = |n: u32| {
        format!(
            r#"<script type="application/ld+json">{{"@type":"MusicEvent","name":"Night {n}","url":"https://gigs.example/concerts/{n}","startDate":"2027-05-0{n}T19:30:00"}}</script>"#
        )
    };
    format!(
        r#"<html><head><title>Venue</title><meta property="og:title" content="Venue"><script type="application/ld+json">{graph}</script>{}{}</head><body></body></html>"#,
        event(1),
        event(2)
    )
}

#[test]
fn pruning_a_large_nested_page_copies_nothing() {
    let url = "https://gigs.example/venue";
    let body = page();
    let size = body.len();
    assert!(size > 850_000, "{size}");
    let fetched = Fetched {
        permanent_redirect: false,
        truncated: false,
        validators: Default::default(),
        url: url.parse().unwrap(),
        content_type: "text/html".into(),
        status: 200,
        retry_after_secs: 60,
        location: None,
        body: body.into_bytes(),
    };
    let before = ALLOCATED.load(Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let started = common::thread_cpu_time();
    let doc = web::extract(url, &fetched, 3600).unwrap().doc;
    let elapsed = common::thread_cpu_time() - started;
    let allocated = ALLOCATED.load(Ordering::Relaxed) - before;
    let peak = PEAK.load(Ordering::Relaxed)
        - LIVE
            .load(Ordering::Relaxed)
            .min(PEAK.load(Ordering::Relaxed));
    eprintln!(
        "page {size} bytes: allocated {allocated} bytes ({}x), peak live {peak} bytes, {elapsed:?} of CPU time",
        allocated / size
    );
    // The list entries are left out, and the nested graph was read (its innermost node is the
    // card, as a graph's is): no event, no start.
    assert_eq!(doc.type_name, "WebPage");
    assert_eq!(doc.name.as_deref(), Some("Leaf"), "{:?}", doc.name);
    assert_eq!(doc.description.as_deref(), Some("end"));
    assert!(doc.start.is_none());
    // Within a small multiple of the page: parsing it and decoding its strings, no copies of
    // the nested graphs at each level (a copy of the subtree at every level costs another
    // twenty-five times the page here, two hundred on the page the review measured).
    assert!(
        allocated < 40 * size,
        "allocated {allocated} bytes for {size}"
    );
    // Of this thread's CPU time: a bound load never inflates.
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
}
